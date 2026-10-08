//! The benchmark harness runs end to end and produces sane rows (needs Chromium).

#[test]
fn bench_harness_produces_plausible_rows() {
    if glyph_server::browser::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("rows.json");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_glyph"))
        .args([
            "bench",
            "--profile",
            "balanced",
            "--pages",
            "static",
            "ticker",
            "--seconds",
            "4",
            "--warmup",
            "2",
            "--json",
        ])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(rows.len(), 4, "2 pages x idle+scroll");
    let get = |page: &str, sc: &str| {
        rows.iter()
            .find(|r| r["page"] == page && r["scenario"] == sc)
            .unwrap_or_else(|| panic!("{page}/{sc} missing"))
    };
    for r in &rows {
        assert!(r["chromium_procs"].as_u64().unwrap() >= 3, "{r}");
        assert!(r["chromium_rss_sum_mb"].as_f64().unwrap() > 100.0, "{r}");
        assert!(r["seconds"].as_f64().unwrap() >= 3.5, "{r}");
        // glyph itself is small; if this ever reports hundreds of MB the children are being counted again
        if let Some(m) = r["glyph_mem_mb"].as_f64() {
            assert!(
                m < 150.0,
                "glyph process memory looks like it includes Chromium: {r}"
            );
        }
    }
    // a static page that nobody touches sends nothing; a ticking page keeps streaming
    assert_eq!(
        get("static", "idle")["frames_per_min"].as_f64().unwrap(),
        0.0
    );
    assert!(get("ticker", "idle")["frames_per_min"].as_f64().unwrap() > 60.0);
    assert!(get("static", "scroll")["wire_kb_per_min"].as_f64().unwrap() > 0.0);
}
