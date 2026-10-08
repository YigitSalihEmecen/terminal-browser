//! Resource profiles (lean / balanced / full) and how they are applied to a tab.

use anyhow::Result;
use glyph_proto::Profile;
use serde_json::json;

use crate::cdp::Session;

#[derive(Clone, Debug)]
pub struct ProfileCfg {
    pub profile: Profile,
    pub max_fps: f32,
    /// Frame-rate ceiling while video/canvas pixels are on screen (cheap refreshes: no DOM snapshot).
    pub live_fps: f32,
    pub jpeg_quality: u8,
    /// `Emulation.setCPUThrottlingRate` (1 = off). Off in every profile: measured on Chrome 154,
    /// throttling makes an *idle* page burn 55-85 % of a core (the throttle duty-cycles the main
    /// thread), the opposite of its purpose. Available as `--cpu-throttle` for experiments.
    pub cpu_throttle: f64,
    pub block_images: bool,
    pub block_fonts: bool,
    pub block_media: bool,
    pub block_trackers: bool,
    /// Background tabs: freeze, or (lean) discard after this many seconds.
    pub discard_after_secs: Option<u64>,
    /// Chromium site isolation; turning it off saves RAM, see DESIGN.md.
    pub site_isolation: bool,
    /// Refuse requests (documents, XHR, fetch) to loopback / private / link-local hosts.
    pub block_private: bool,
}

impl ProfileCfg {
    pub fn for_profile(p: Profile) -> Self {
        match p {
            Profile::Lean => Self {
                profile: p,
                max_fps: 4.0,
                live_fps: 6.0,
                jpeg_quality: 70,
                cpu_throttle: 1.0,
                block_images: true,
                block_fonts: true,
                block_media: true,
                block_trackers: true,
                discard_after_secs: Some(60),
                site_isolation: false,
                block_private: false,
            },
            Profile::Balanced => Self {
                profile: p,
                max_fps: 10.0,
                live_fps: 15.0,
                jpeg_quality: 80,
                cpu_throttle: 1.0,
                block_images: false,
                block_fonts: true,
                block_media: false,
                block_trackers: true,
                discard_after_secs: None,
                site_isolation: true,
                block_private: false,
            },
            Profile::Full => Self {
                profile: p,
                max_fps: 20.0,
                live_fps: 20.0,
                jpeg_quality: 85,
                cpu_throttle: 1.0,
                block_images: false,
                block_fonts: false,
                block_media: false,
                block_trackers: false,
                discard_after_secs: None,
                site_isolation: true,
                block_private: false,
            },
        }
    }

    /// Extra Chromium flags this profile wants at launch.
    pub fn chrome_flags(&self) -> Vec<String> {
        let mut f = vec![];
        if !self.site_isolation {
            // (merged with the launcher's own feature list into a single switch)
            f.push("--disable-features=IsolateOrigins,site-per-process".into());
            f.push("--disable-site-isolation-trials".into());
            f.push("--process-per-site".into());
        }
        if self.profile == Profile::Lean {
            // Measured on macOS arm64 (balanced → these): ~-45 MB and one process fewer.
            f.push("--in-process-gpu".into());
            f.push("--disable-features=SpareRendererForSitePerProcess,SpareRenderer".into());
            f.push("--renderer-process-limit=2".into());
            f.push("--js-flags=--max-old-space-size=256".into());
        }
        f
    }

    /// Fetch interception patterns: only matching requests are paused (and failed).
    pub fn fetch_patterns(&self, client_wants_images: bool) -> Vec<serde_json::Value> {
        let mut p = vec![];
        if self.block_images && !client_wants_images {
            p.push(json!({ "resourceType": "Image" }));
        }
        if self.block_fonts {
            p.push(json!({ "resourceType": "Font" }));
        }
        if self.block_media {
            p.push(json!({ "resourceType": "Media" }));
        }
        if self.block_trackers {
            for d in TRACKER_DOMAINS {
                // with and without an explicit port, apex and subdomains
                for pat in [
                    format!("*://{d}/*"),
                    format!("*://*.{d}/*"),
                    format!("*://{d}:*/*"),
                    format!("*://*.{d}:*/*"),
                ] {
                    p.push(json!({ "urlPattern": pat }));
                }
            }
        }
        if self.block_private {
            for t in ["Document", "XHR", "Fetch"] {
                p.push(json!({ "resourceType": t }));
            }
        }
        if self.block_trackers || self.block_images {
            // browsers fetch a favicon nobody here will ever see
            p.push(json!({ "urlPattern": "*://*/favicon.ico" }));
        }
        p
    }

    pub async fn apply(&self, s: &Session, client_wants_images: bool) -> Result<()> {
        if self.cpu_throttle > 1.0 {
            s.send(
                "Emulation.setCPUThrottlingRate",
                json!({ "rate": self.cpu_throttle }),
            )
            .await?;
        }
        let patterns = self.fetch_patterns(client_wants_images);
        if !patterns.is_empty() {
            s.send("Fetch.enable", json!({ "patterns": patterns }))
                .await?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Fail,
}

/// Is `host` (already lowercased) an IP literal or name that points inside the machine/LAN?
pub fn is_private_host(host: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let host = host.trim_matches(['[', ']']);
    fn v4(a: Ipv4Addr) -> bool {
        let o = a.octets();
        a.is_loopback()
            || a.is_private()
            || a.is_link_local()
            || a.is_unspecified()
            || a.is_broadcast()
            || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
            || o[0] == 0
    }
    fn v6(a: Ipv6Addr) -> bool {
        if let Some(m) = a.to_ipv4_mapped() {
            return v4(m);
        }
        let seg0 = a.segments()[0];
        a.is_loopback()
            || a.is_unspecified()
            || (seg0 & 0xfe00) == 0xfc00
            || (seg0 & 0xffc0) == 0xfe80
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => v4(a),
        Ok(IpAddr::V6(a)) => v6(a),
        Err(_) => {
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".internal")
                || host.ends_with(".lan")
                || host.ends_with(".home.arpa")
                || !host.contains('.') // single-label names resolve via the search domain
        }
    }
}

pub fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

pub fn is_tracker_url(url: &str) -> bool {
    let Some(h) = url_host(url) else { return false };
    TRACKER_DOMAINS
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")))
}

impl ProfileCfg {
    /// What to do with a paused request. Patterns decide which requests we *see*; this decides
    /// their fate, so the rules live in one testable place.
    pub fn verdict(&self, resource_type: &str, url: &str, client_wants_images: bool) -> Verdict {
        use Verdict::*;
        if self.block_private
            && matches!(resource_type, "Document" | "XHR" | "Fetch")
            && url_host(url).is_some_and(|h| is_private_host(&h))
        {
            return Fail;
        }
        match resource_type {
            "Image" if self.block_images && !client_wants_images => Fail,
            "Font" if self.block_fonts => Fail,
            "Media" if self.block_media => Fail,
            // never block top-level navigations because a tracker pattern happened to match
            "Document" => Continue,
            _ if self.block_trackers && is_tracker_url(url) => Fail,
            _ if (self.block_trackers || self.block_images) && url.ends_with("/favicon.ico") => {
                Fail
            }
            _ => Continue,
        }
    }
}

/// A deliberately short list of the highest-volume ad/analytics hosts. Matching is by suffix
/// (`*://*.d/*` and `*://d/*`). Extend via config; this is not a full filter list.
pub const TRACKER_DOMAINS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-analytics.com",
    "googletagmanager.com",
    "googletagservices.com",
    "adservice.google.com",
    "facebook.net",
    "connect.facebook.net",
    "hotjar.com",
    "segment.io",
    "segment.com",
    "mixpanel.com",
    "amplitude.com",
    "fullstory.com",
    "newrelic.com",
    "nr-data.net",
    "scorecardresearch.com",
    "quantserve.com",
    "taboola.com",
    "outbrain.com",
    "criteo.com",
    "criteo.net",
    "adnxs.com",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "amazon-adsystem.com",
    "moatads.com",
    "adsrvr.org",
    "casalemedia.com",
    "clarity.ms",
    "mouseflow.com",
    "crazyegg.com",
    "optimizely.com",
    "intercom.io",
    "ads-twitter.com",
    "analytics.tiktok.com",
    "snap.licdn.com",
    "bat.bing.com",
    "static.ads-twitter.com",
    "sentry.io",
    "bugsnag.com",
    "datadoghq.com",
    "appsflyer.com",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts() {
        for h in [
            "localhost",
            "foo.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "[::1]",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.1.1",
            "printer",
            "nas.local",
            "db.internal",
        ] {
            assert!(is_private_host(h), "{h} should be private");
        }
        for h in [
            "example.org",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "1.1.1.1",
            "sub.example.co.uk",
        ] {
            assert!(!is_private_host(h), "{h} should be public");
        }
    }

    #[test]
    fn verdicts() {
        let mut p = ProfileCfg::for_profile(Profile::Lean);
        p.block_private = true;
        assert_eq!(
            p.verdict("Document", "http://169.254.169.254/latest/meta-data", false),
            Verdict::Fail
        );
        assert_eq!(
            p.verdict("XHR", "http://localhost:9222/json", false),
            Verdict::Fail
        );
        assert_eq!(
            p.verdict("Document", "https://example.org/", false),
            Verdict::Continue
        );
        assert_eq!(
            p.verdict("Image", "https://example.org/a.png", false),
            Verdict::Fail
        );
        assert_eq!(
            p.verdict("Image", "https://example.org/a.png", true),
            Verdict::Continue
        );
        assert_eq!(
            p.verdict("Font", "https://example.org/a.woff2", true),
            Verdict::Fail
        );
        assert_eq!(
            p.verdict(
                "Script",
                "https://www.google-analytics.com/analytics.js",
                false
            ),
            Verdict::Fail
        );
        assert_eq!(
            p.verdict("Script", "https://notgoogle-analytics.com/a.js", false),
            Verdict::Continue
        );
        // a tracker domain as a top-level page is still reachable
        assert_eq!(
            p.verdict("Document", "https://segment.com/", false),
            Verdict::Continue
        );
        assert_eq!(
            p.verdict("Other", "https://example.org/favicon.ico", false),
            Verdict::Fail
        );
        let full = ProfileCfg::for_profile(Profile::Full);
        assert_eq!(
            full.verdict(
                "Script",
                "https://www.google-analytics.com/analytics.js",
                false
            ),
            Verdict::Continue
        );
        assert_eq!(
            full.verdict("Document", "http://127.0.0.1/", false),
            Verdict::Continue,
            "local use is allowed"
        );
    }
}
