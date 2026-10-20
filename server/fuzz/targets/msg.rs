//! The messages between the front, the banks and the banks' folder, read from any bytes: an error
//! or a value that writes back to something that reads the same (a message from another release
//! keeps what it carries).
use super::*;

fn again<T: DeserializeOwned + Serialize + PartialEq + std::fmt::Debug>(data: &[u8]) {
    if let Ok(m) = parse::<T>(data) {
        let back = text(&m);
        let second: T = parse(back.as_bytes()).unwrap_or_else(|e| panic!("a message written does not read: {e}: {back}"));
        assert_eq!(second, m);
        assert_eq!(text(&second), back, "writing is not a fixpoint");
    }
}

pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else { return };
    match which % 12 {
        0 => again::<BankInfo>(rest),
        1 => again::<Hello>(rest),
        2 => again::<Restore>(rest),
        3 => again::<Ready>(rest),
        4 => again::<SnapshotNote>(rest),
        5 => again::<SnapshotHead>(rest),
        6 => again::<Connected>(rest),
        7 => again::<AdminCall>(rest),
        8 => again::<AdminAnswer>(rest),
        9 => again::<Ops>(rest),
        10 => {
            again::<RolledBack>(rest);
            again::<Fallback>(rest);
        }
        _ => again::<ChatEvent>(rest),
    }
}
