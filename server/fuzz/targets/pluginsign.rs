//! plugin.sig, public and private key files, and the trusted-keys bookkeeping: a file that is not
//! a signature of exactly these files by exactly a trusted key is never `Trusted`.
use super::*;
use std::sync::OnceLock;

static KEY: OnceLock<(String, Vec<u8>)> = OnceLock::new();

pub fn run(data: &[u8]) {
    let (private, public) = KEY.get_or_init(|| {
        let (private, public) = generate().expect("a key");
        (private, parse_public(&public).expect("its own public key"))
    });
    let Some((&which, rest)) = data.split_first() else { return };
    let text = String::from_utf8_lossy(rest);
    match which % 4 {
        0 => {
            let _ = parse_public(&text);
            let _ = load_private(&text);
        }
        1 => {
            // The signature file of data, judged against files whose digest is the first line.
            let dir = std::env::temp_dir().join(format!("pcds-fuzz-sign-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let (digest, body) = text.split_once('\n').unwrap_or(("00", &text));
            std::fs::write(dir.join(FILE), body.as_bytes()).unwrap();
            let mut keys = Keys::default();
            // Nobody trusted: nothing is Trusted, whatever the file says.
            assert!(!matches!(check(&dir, digest, &keys), Signed::Trusted(_)));
            keys.trust(public, "k", vec![], false).unwrap();
            let verdict = check(&dir, digest, &keys);
            if let Signed::Trusted(k) = &verdict {
                // Only the real signature of exactly this digest gets here.
                let again = sign(private, digest).unwrap();
                let (a, b) = (serde_json::from_str::<Sig>(body).unwrap(), serde_json::from_str::<Sig>(&again).unwrap());
                assert_eq!((a.signature, a.digest, a.public_key), (b.signature, b.digest, b.public_key), "{k:?}");
            }
            let _ = std::fs::remove_file(dir.join(FILE));
        }
        2 => {
            // A signature of a digest from the data, checked and then damaged.
            let digest = crate::util::to_hex(&rest[..rest.len().min(32)]);
            let good = sign(private, &digest).unwrap();
            let dir = std::env::temp_dir().join(format!("pcds-fuzz-sign-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(FILE), &good).unwrap();
            let mut keys = Keys::default();
            keys.trust(public, "k", vec![], false).unwrap();
            assert!(matches!(check(&dir, &digest, &keys), Signed::Trusted(_)), "a signature made here does not hold");
            let mut damaged = good.into_bytes();
            if !damaged.is_empty() {
                let at = rest.get(33).copied().unwrap_or(0) as usize % damaged.len();
                damaged[at] ^= rest.get(34).copied().unwrap_or(1).max(1);
            }
            std::fs::write(dir.join(FILE), &damaged).unwrap();
            if let Signed::Trusted(_) = check(&dir, &digest, &keys) {
                // Only a change that leaves the meaning (white space, or hex case) may pass.
                let sig = serde_json::from_slice::<Sig>(&damaged).expect("it parsed to be trusted");
                let ok = sign(private, &digest).unwrap();
                let ok = serde_json::from_str::<Sig>(&ok).unwrap();
                assert_eq!((sig.signature.to_ascii_lowercase(), sig.public_key.to_ascii_lowercase(), sig.digest), (ok.signature, ok.public_key, ok.digest));
            }
            let _ = std::fs::remove_file(dir.join(FILE));
        }
        _ => {
            let mut keys = Keys::default();
            let _ = keys.trust(&rest[..rest.len().min(40)], &text, text.split(',').map(String::from).collect(), rest.len() % 2 == 0);
            let _ = keys.find(&text);
            let _ = keys.revoke(&text);
            let _ = keys.find(&text);
        }
    }
}
