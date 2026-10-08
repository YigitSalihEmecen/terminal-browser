//! Resource profiles (lean / balanced / full) and how they are applied to a tab.

use anyhow::Result;
use glyph_proto::Profile;
use serde_json::json;

use crate::cdp::Session;

#[derive(Clone, Debug)]
pub struct ProfileCfg {
    pub profile: Profile,
    pub max_fps: f32,
    pub jpeg_quality: u8,
    /// `Emulation.setCPUThrottlingRate` (1 = off).
    pub cpu_throttle: f64,
    pub block_images: bool,
    pub block_fonts: bool,
    pub block_media: bool,
    pub block_trackers: bool,
    /// Background tabs: freeze, or (lean) discard after this many seconds.
    pub discard_after_secs: Option<u64>,
    /// Chromium site isolation; turning it off saves RAM, see DESIGN.md.
    pub site_isolation: bool,
}

impl ProfileCfg {
    pub fn for_profile(p: Profile) -> Self {
        match p {
            Profile::Lean => Self {
                profile: p,
                max_fps: 4.0,
                jpeg_quality: 30,
                cpu_throttle: 2.0,
                block_images: true,
                block_fonts: true,
                block_media: true,
                block_trackers: true,
                discard_after_secs: Some(60),
                site_isolation: false,
            },
            Profile::Balanced => Self {
                profile: p,
                max_fps: 10.0,
                jpeg_quality: 45,
                cpu_throttle: 1.0,
                block_images: false,
                block_fonts: true,
                block_media: false,
                block_trackers: true,
                discard_after_secs: None,
                site_isolation: true,
            },
            Profile::Full => Self {
                profile: p,
                max_fps: 20.0,
                jpeg_quality: 60,
                cpu_throttle: 1.0,
                block_images: false,
                block_fonts: false,
                block_media: false,
                block_trackers: false,
                discard_after_secs: None,
                site_isolation: true,
            },
        }
    }

    /// Extra Chromium flags this profile wants at launch.
    pub fn chrome_flags(&self) -> Vec<String> {
        let mut f = vec![];
        if !self.site_isolation {
            f.push("--disable-features=IsolateOrigins,site-per-process".into());
            f.push("--process-per-site".into());
        }
        if self.profile == Profile::Lean {
            f.push("--renderer-process-limit=3".into());
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
