//! The updater's tag and version parsing, the compatibility check on hostile bank.json/HELLO
//! JSON, and the release JSON's assets.
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data).into_owned();
    let (a, b) = text.split_once('\n').unwrap_or((&text, ""));
    let _ = check_tag(a);
    let _ = tag(a);
    let _ = newer(a, b);
    // Order is a total order: antisymmetric.
    assert!(!(newer(a, b) && newer(b, a)));
    assert!(!newer(a, a));
    let _ = local(a);
    let (x, y) = (serde_json::from_str::<Value>(a).unwrap_or(Value::Null), serde_json::from_str::<Value>(b).unwrap_or(Value::Null));
    let (bx, by) = (serde_json::from_value::<crate::msg::BankInfo>(x.clone()), serde_json::from_value::<crate::msg::BankInfo>(y));
    if let (Ok(bx), Ok(by)) = (&bx, &by) {
        let _ = compatible(bx, Some(by));
    }
    if let Ok(bx) = &bx {
        let _ = compatible(bx, None);
    }
    // A release's JSON, as lookup() reads it (without the HTTP).
    let release = &x;
    let _ = release["tag_name"].as_str().map(check_tag);
    let _ = release["assets"].as_array().into_iter().flatten().find(|a| a["name"] == "x").and_then(|a| a["browser_download_url"].as_str());
}
