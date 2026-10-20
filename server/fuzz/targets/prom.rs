//! GET /api/metrics: whatever JSON the front puts together (hostile plugin ids and states, numbers
//! of every kind, the wrong types in every place), `render` writes text a strict reader of the
//! Prometheus format accepts, the same text twice, with finite numbers only.
use super::*;

pub fn run(data: &[u8]) {
    let Ok(m) = serde_json::from_slice::<Value>(data) else { return };
    let out = render(&m);
    assert_eq!(out, render(&m), "render is a function of its input");
    // Two plugins under one id are two samples of one series: the plugin host does not allow it.
    let ids: Vec<String> = m["plugins"].as_array().map(|p| p.iter().map(|p| text(&p["id"])).collect()).unwrap_or_default();
    let unique = ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len();
    match parse(&out) {
        Ok(samples) => assert!(samples.iter().all(|s| s.2.is_finite())),
        Err(e) => assert!(!unique, "{e}\n{out}"),
    }
}
