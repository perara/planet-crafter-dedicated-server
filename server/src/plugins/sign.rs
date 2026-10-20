//! Signed plugins: a publisher signs what a plugin is, and an admin who trusts the publisher's key
//! does not have to approve each file (docs/plugins.md, "Signing"). Trust is only ever *added* by a
//! signature: a plugin without one, or whose signature does not hold, is judged by the digest the
//! admin pinned, as before.
//!
//! - `plugin.sig` in the plugin's folder: JSON `{"publicKey", "signature", "digest"}`, hex. The
//!   signature is Ed25519 (ring, which the server has for HMAC and TLS) over `SIGNED_PREFIX` and the
//!   plugin's digest (sha256 of the manifest, a zero byte and the entry: approvals.rs), so it covers
//!   the manifest (the capabilities, the allow list, the version) and the module or the rules.
//!   The file is checked against the files as they are *now*, at every load and every reload.
//! - `trusted-keys.json` beside `approved.json`: the keys the admin trusts, each with a *trust
//!   grant*: the most capabilities it vouches for. A plugin signed by a trusted key whose
//!   capabilities are all within the grant runs without any approval; one that asks for more waits
//!   for the admin for the rest, and once approved follows that publisher's updates (the approval
//!   records the signer) as long as it asks for nothing beyond what the key and the approval cover.
//!   A key the admin removes (`pcds plugin untrust`) is recorded as revoked, and its plugins stop at
//!   the next look at the folder.
//! - Keys are made by `pcds plugin keygen`; the private key is a file the publisher keeps (0600).

use std::path::Path;

use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use super::state::{hex, unhex};

pub const FILE: &str = "plugin.sig";
pub const KEYS_FILE: &str = "trusted-keys.json";
/// Text signed in front of the digest, so a signature made for this is good for nothing else.
const SIGNED_PREFIX: &str = "pcds-plugin-v1:";
const PRIVATE_HEADER: &str = "pcds-plugin-private-key-v1";
const PUBLIC_HEADER: &str = "pcds-plugin-public-key-v1";
/// A signature file is a few hundred bytes; more is not read.
const MAX_SIG_BYTES: u64 = 4096;

/// A key's short name: the first 16 bytes of sha256 of the public key, hex.
pub fn key_id(public_key: &[u8]) -> String {
    hex(&ring::digest::digest(&ring::digest::SHA256, public_key).as_ref()[..16])
}

fn message(digest: &str) -> Vec<u8> {
    format!("{SIGNED_PREFIX}{digest}").into_bytes()
}

// ---- making keys and signatures (pcds plugin keygen / sign) --------------------------------------

/// A new key pair: the private key's file text and the public key's.
pub fn generate() -> Result<(String, String), String> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| "the system has no random numbers to make a key from")?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|e| e.to_string())?;
    let public = pair.public_key().as_ref();
    Ok((format!("{PRIVATE_HEADER}\n{}\n", hex(pkcs8.as_ref())), format!("{PUBLIC_HEADER}\n{}\n# id {}\n", hex(public), key_id(public))))
}

fn load_private(text: &str) -> Result<Ed25519KeyPair, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    if lines.next().map(str::trim) != Some(PRIVATE_HEADER) {
        return Err(format!("not a plugin private key (the first line is {PRIVATE_HEADER})"));
    }
    let body = unhex(lines.next().unwrap_or("").trim()).map_err(|_| "the private key is not hex".to_string())?;
    Ed25519KeyPair::from_pkcs8(&body).map_err(|_| "the private key is not a valid Ed25519 key".to_string())
}

/// The text of a `plugin.sig` for the plugin whose digest is `digest`.
pub fn sign(private_key_file: &str, digest: &str) -> Result<String, String> {
    let pair = load_private(private_key_file)?;
    let signature = pair.sign(&message(digest));
    let sig = Sig { public_key: hex(pair.public_key().as_ref()), signature: hex(signature.as_ref()), digest: digest.to_string() };
    Ok(format!("{}\n", serde_json::to_string_pretty(&sig).map_err(|e| e.to_string())?))
}

/// A public key from what an admin has: the hex, or the text of a `.pub` file.
pub fn parse_public(text: &str) -> Result<Vec<u8>, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let first = lines.next().ok_or("no key")?;
    let body = if first == PUBLIC_HEADER { lines.next().ok_or("the public key file has no key")? } else { first };
    let bytes = unhex(body).map_err(|_| "a public key is 64 hex digits (or the text of a .pub file)".to_string())?;
    if bytes.len() != 32 {
        return Err("a public key is 64 hex digits (32 bytes)".into());
    }
    Ok(bytes)
}

// ---- reading a signature -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Sig {
    pub public_key: String,
    pub signature: String,
    pub digest: String,
}

/// What a plugin's folder says about who made it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Signed {
    /// No `plugin.sig`.
    #[default]
    No,
    /// There is one, and it does not hold: said why. The plugin is judged as if it had none.
    Broken(String),
    /// A good signature, by a key this server does not trust.
    Unknown { id: String },
    /// A good signature, by a key the admin removed.
    Revoked { id: String },
    /// A good signature by a key the admin trusts.
    Trusted(TrustedKey),
}

impl Signed {
    pub fn state(&self) -> &'static str {
        match self {
            Signed::No => "none",
            Signed::Broken(_) => "invalid",
            Signed::Unknown { .. } => "untrusted",
            Signed::Revoked { .. } => "revoked",
            Signed::Trusted(_) => "trusted",
        }
    }
    pub fn key_id(&self) -> Option<&str> {
        match self {
            Signed::Unknown { id } | Signed::Revoked { id } => Some(id),
            Signed::Trusted(k) => Some(&k.id),
            _ => None,
        }
    }
    pub fn trusted(&self) -> Option<&TrustedKey> {
        match self {
            Signed::Trusted(k) => Some(k),
            _ => None,
        }
    }

    /// One sentence for the admin who is about to approve the plugin (`requested`: what it asks for).
    pub fn describe(&self, requested: &[String]) -> String {
        match self {
            Signed::No => "unsigned: approved by the digest of its files".to_string(),
            Signed::Broken(why) => format!("{why} (treated as unsigned)"),
            Signed::Unknown { id } => format!("signed by the key {id}, which you do not trust (pcds plugin trust)"),
            Signed::Revoked { id } => format!("signed by the key {id}, which was revoked"),
            Signed::Trusted(k) => {
                let vouched: Vec<&str> = requested.iter().filter(|c| k.grant.contains(c)).map(String::as_str).collect();
                let rest: Vec<&str> = requested.iter().filter(|c| !k.grant.contains(c)).map(String::as_str).collect();
                format!("signed by {} ({}), whom you trust for: {}; needs your approval for: {}", k.name, k.id, if vouched.is_empty() { "none of what it asks".to_string() } else { vouched.join(", ") }, if rest.is_empty() { "nothing".to_string() } else { rest.join(", ") })
            }
        }
    }
}

/// Checks the `plugin.sig` in `folder` against `digest` (the digest of the files as they are read now).
pub fn check(folder: &Path, digest: &str, keys: &Keys) -> Signed {
    use std::io::Read;
    let path = folder.join(FILE);
    let mut text = String::new();
    match std::fs::File::open(&path) {
        Ok(f) => {
            if let Err(e) = f.take(MAX_SIG_BYTES + 1).read_to_string(&mut text) {
                return Signed::Broken(format!("{FILE}: {e}"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Signed::No,
        Err(e) => return Signed::Broken(format!("{FILE}: {e}")),
    }
    if text.len() as u64 > MAX_SIG_BYTES {
        return Signed::Broken(format!("{FILE}: over {MAX_SIG_BYTES} bytes"));
    }
    let broken = |why: &str| Signed::Broken(format!("{FILE}: {why}"));
    let Ok(sig) = serde_json::from_str::<Sig>(&text) else { return broken("not a signature file") };
    let (Ok(public), Ok(signature)) = (unhex(&sig.public_key), unhex(&sig.signature)) else { return broken("the key or the signature is not hex") };
    if public.len() != 32 || signature.len() != 64 {
        return broken("a key is 32 bytes and a signature 64");
    }
    if sig.digest != digest {
        return broken("it was made for other files than these (the manifest or the module changed after it was signed)");
    }
    if UnparsedPublicKey::new(&ED25519, &public).verify(&message(digest), &signature).is_err() {
        return broken("the signature does not match the files");
    }
    let id = key_id(&public);
    if let Some(k) = keys.keys.iter().find(|k| k.id == id) {
        Signed::Trusted(k.clone())
    } else if keys.revoked.iter().any(|r| r.id == id) {
        Signed::Revoked { id }
    } else {
        Signed::Unknown { id }
    }
}

// ---- the keys an admin trusts --------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TrustedKey {
    pub id: String,
    pub public_key: String,
    /// What the admin calls the publisher.
    pub name: String,
    /// The trust grant: the capabilities this key vouches for. A plugin it signed asks for no
    /// approval for these; anything it asks for beyond them waits for the admin.
    pub grant: Vec<String>,
    /// Unix seconds.
    pub added_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Revoked {
    pub id: String,
    pub name: String,
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    #[serde(default)]
    pub keys: Vec<TrustedKey>,
    #[serde(default)]
    pub revoked: Vec<Revoked>,
}

impl Keys {
    /// The file in `dir`; none there is no trust. One that is there and does not read is an error:
    /// no key is trusted then (an unreadable list never means everything is trusted).
    pub fn load(dir: &Path) -> Result<Keys, String> {
        let path = dir.join(KEYS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Keys::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Keys::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join(KEYS_FILE);
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::save::write_atomic(&path.to_string_lossy(), format!("{text}\n").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Trusts a key (or changes its name and trust grant). `force`: even one that was revoked.
    pub fn trust(&mut self, public_key: &[u8], name: &str, grant: Vec<String>, force: bool) -> Result<TrustedKey, String> {
        let id = key_id(public_key);
        if let Some(r) = self.revoked.iter().find(|r| r.id == id) {
            if !force {
                return Err(format!("the key {id} was revoked (as {}); --force trusts it again", r.name));
            }
            self.revoked.retain(|r| r.id != id);
        }
        if let Some(g) = grant.iter().find(|g| !pcds_plugin_api::caps::known(g)) {
            return Err(format!("{g:?} is not a capability (docs/plugins-api.md lists them)"));
        }
        let name = if name.trim().is_empty() { id.clone() } else { name.trim().chars().take(64).collect() };
        let key = TrustedKey { id: id.clone(), public_key: hex(public_key), name, grant, added_at: super::approvals::now() };
        self.keys.retain(|k| k.id != id);
        self.keys.push(key.clone());
        Ok(key)
    }

    /// Removes a key and remembers it as revoked; the key as it was, or none if it was not trusted.
    pub fn revoke(&mut self, id: &str) -> Option<TrustedKey> {
        let at = self.keys.iter().position(|k| k.id == id)?;
        let key = self.keys.remove(at);
        self.revoked.retain(|r| r.id != id);
        self.revoked.push(Revoked { id: key.id.clone(), name: key.name.clone(), at: super::approvals::now() });
        Some(key)
    }

    /// A key by its id, by a start of it of 8 characters or more (as the listing shows it), or by
    /// the name it was trusted under.
    pub fn find(&self, id_or_prefix: &str) -> Result<&TrustedKey, String> {
        let hits: Vec<&TrustedKey> = self.keys.iter().filter(|k| k.id == id_or_prefix || (id_or_prefix.len() >= 8 && k.id.starts_with(id_or_prefix)) || k.name.eq_ignore_ascii_case(id_or_prefix)).collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("no trusted key {id_or_prefix} (its id or the name it was trusted under; the Plugins page lists the trusted keys)")),
            _ => Err(format!("{id_or_prefix} names more than one key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = super::super::tests::fresh_dir(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pair() -> (String, Vec<u8>) {
        let (private, public) = generate().unwrap();
        (private, parse_public(&public).unwrap())
    }

    #[test]
    fn a_signature_holds_for_the_files_it_was_made_for_and_for_no_others() {
        let d = dir("sign-hold");
        let (private, public) = pair();
        let digest = "ab".repeat(32);
        std::fs::write(d.join(FILE), sign(&private, &digest).unwrap()).unwrap();
        let mut keys = Keys::default();
        // Good, but nobody trusts the key.
        assert_eq!(check(&d, &digest, &keys), Signed::Unknown { id: key_id(&public) });
        keys.trust(&public, "Acme", vec!["chat.say".into()], false).unwrap();
        assert!(matches!(check(&d, &digest, &keys), Signed::Trusted(k) if k.name == "Acme" && k.grant == vec!["chat.say".to_string()]));
        // The files changed after it was signed: another digest.
        assert!(matches!(check(&d, &"cd".repeat(32), &keys), Signed::Broken(why) if why.contains("other files")));
        // Revoked: said so, and not trusted.
        keys.revoke(&key_id(&public)).unwrap();
        assert_eq!(check(&d, &digest, &keys), Signed::Revoked { id: key_id(&public) });
        assert!(keys.trust(&public, "Acme", vec![], false).unwrap_err().contains("--force"));
        keys.trust(&public, "Acme", vec![], true).unwrap();
        assert!(matches!(check(&d, &digest, &keys), Signed::Trusted(_)) && keys.revoked.is_empty());
        // No file: unsigned.
        std::fs::remove_file(d.join(FILE)).unwrap();
        assert_eq!(check(&d, &digest, &keys), Signed::No);
    }

    #[test]
    fn a_forged_swapped_cut_or_garbled_signature_is_broken_never_trusted() {
        let d = dir("sign-forged");
        let (private, public) = pair();
        let (other_private, _) = pair();
        let digest = "12".repeat(32);
        let mut keys = Keys::default();
        keys.trust(&public, "Acme", vec![], false).unwrap();
        let good = sign(&private, &digest).unwrap();
        let sig: Sig = serde_json::from_str(&good).unwrap();
        let broken = |text: String| {
            std::fs::write(d.join(FILE), text).unwrap();
            matches!(check(&d, &digest, &keys), Signed::Broken(_))
        };
        // The right key's name on another key's signature; one flipped bit; the signature of another digest.
        let other: Sig = serde_json::from_str(&sign(&other_private, &digest).unwrap()).unwrap();
        assert!(broken(serde_json::to_string(&Sig { signature: other.signature.clone(), ..sig.clone() }).unwrap()));
        let mut bytes = unhex(&sig.signature).unwrap();
        bytes[10] ^= 1;
        assert!(broken(serde_json::to_string(&Sig { signature: hex(&bytes), ..sig.clone() }).unwrap()));
        let elsewhere: Sig = serde_json::from_str(&sign(&private, &"34".repeat(32)).unwrap()).unwrap();
        assert!(broken(serde_json::to_string(&Sig { signature: elsewhere.signature, ..sig.clone() }).unwrap()), "a signature of another plugin's digest");
        // Garbage in every shape.
        for text in ["", "{}", "not json", "{\"publicKey\":1}", &good[..good.len() / 2], &good.replace(&sig.public_key, &"zz".repeat(32)), &good.replace(&sig.public_key, "abcd"), &"x".repeat(10_000)] {
            assert!(broken(text.to_string()), "{text:.40}");
        }
        // A signature file that is a directory, or unreadable, is broken too.
        std::fs::remove_file(d.join(FILE)).unwrap();
        std::fs::create_dir(d.join(FILE)).unwrap();
        assert!(matches!(check(&d, &digest, &keys), Signed::Broken(_)));
        // The good one still holds.
        std::fs::remove_dir(d.join(FILE)).unwrap();
        std::fs::write(d.join(FILE), good).unwrap();
        assert!(matches!(check(&d, &digest, &keys), Signed::Trusted(_)));
    }

    #[test]
    fn keys_are_made_read_and_named() {
        let (private, public) = generate().unwrap();
        assert!(private.starts_with(PRIVATE_HEADER) && public.starts_with(PUBLIC_HEADER));
        let bytes = parse_public(&public).unwrap();
        assert_eq!(parse_public(&hex(&bytes)).unwrap(), bytes, "the hex alone is a key too");
        assert!(public.contains(&format!("# id {}", key_id(&bytes))));
        assert!(parse_public("abcd").is_err() && parse_public(&"zz".repeat(32)).is_err() && parse_public("").is_err());
        assert!(sign("not a key", "00").is_err() && sign(&format!("{PRIVATE_HEADER}\nzz\n"), "00").is_err());
        let mut keys = Keys::default();
        let k = keys.trust(&bytes, "", vec![], false).unwrap();
        assert_eq!(k.name, k.id, "an unnamed key is called by its id");
        assert!(keys.trust(&bytes, "x", vec!["fly.away".into()], false).unwrap_err().contains("not a capability"));
        assert_eq!(keys.find(&k.id[..8]).unwrap().id, k.id);
        assert!(keys.find(&k.id[..4]).is_err() && keys.find("00000000").is_err());
        // The file round-trips and an unreadable one is an error, not "no keys".
        let d = dir("sign-keys");
        keys.save(&d).unwrap();
        assert_eq!(Keys::load(&d).unwrap(), keys);
        std::fs::write(d.join(KEYS_FILE), "{ nope").unwrap();
        assert!(Keys::load(&d).is_err());
        assert_eq!(Keys::load(&dir("sign-keys2")).unwrap(), Keys::default());
    }
}
