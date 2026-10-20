//! Signed plugins in the host: a publisher's key stands in for the digest of each file, up to what
//! the admin trusts the key for; and nothing a signature says outlives the files it was made for.

use super::sign::{self, Keys, Signed};
use super::tests::{Mock, boot, fresh_dir, module, write_plugin};
use super::*;

const CHAT_SAY: [&str; 2] = ["event.chat", "chat.say"];
const SUBSCRIBE: &str = "[events]\nsubscribe = [\"chat\"]";

/// A key pair: the private key's text, the public key's bytes.
fn pair() -> (String, Vec<u8>) {
    let (private, public) = sign::generate().unwrap();
    (private, sign::parse_public(&public).unwrap())
}

fn say(text_len: usize) -> String {
    module(&format!("(drop (call $say (i32.const 0) (i32.const {text_len}))) (i32.const 0)"), "")
}

/// Writes the plugin and signs what is written.
fn publish(dir: &Path, id: &str, wat: &str, caps: &[&str], private: &str) {
    write_plugin(dir, id, wat, caps, SUBSCRIBE);
    let digest = read_folder(&dir.join(id)).digest;
    std::fs::write(dir.join(id).join(sign::FILE), sign::sign(private, &digest).unwrap()).unwrap();
}

fn trust(dir: &Path, public: &[u8], grant: &[&str]) {
    let mut keys = Keys::load(dir).unwrap();
    keys.trust(public, "Acme", grant.iter().map(|g| g.to_string()).collect(), false).unwrap();
    keys.save(dir).unwrap();
}

fn chat() -> Event<'static> {
    Event::Chat { client: 1, name: "Ann", text: "hi" }
}

fn the(host: &Host) -> &Loaded {
    &host.plugins[0]
}

#[test]
fn a_plugin_signed_by_a_trusted_key_needs_no_approval_for_what_the_key_vouches_for() {
    let dir = fresh_dir("sign-trusted");
    let (private, public) = pair();
    publish(&dir, "notify", &say(5), &CHAT_SAY, &private);
    // The key is not trusted yet: the plugin waits, as any plugin does.
    let host = boot(&dir);
    assert_eq!(the(&host).status, Status::Pending);
    assert!(matches!(the(&host).signed, Signed::Unknown { .. }));
    // Trusted for both capabilities it asks for: it runs, with no approved.json at all.
    trust(&dir, &public, &["event.chat", "chat.say"]);
    let mut host = boot(&dir);
    assert!(the(&host).running(), "{:?}", the(&host).status);
    assert_eq!(the(&host).grant.names(), vec!["event.chat", "chat.say"]);
    assert!(!dir.join("approved.json").exists());
    let mut world = Mock::default();
    host.run(&mut world, &chat(), 0);
    assert_eq!(world.applied.len(), 1, "it said its piece");
    let listing = admin::plugin_json(the(&host));
    assert_eq!((listing["signature"]["state"].as_str(), listing["signature"]["keyName"].as_str()), (Some("trusted"), Some("Acme")));
    // Trusted for less: the rest waits for the admin, and nothing runs meanwhile.
    trust(&dir, &public, &["event.chat"]);
    let host = boot(&dir);
    assert_eq!(the(&host).status, Status::Pending);
    assert_eq!(host.interest, 0);
}

#[test]
fn what_the_key_does_not_vouch_for_is_approved_once_and_the_publishers_updates_follow() {
    let dir = fresh_dir("sign-follow");
    let (private, public) = pair();
    let kick = ["event.chat", "chat.say", "player.kick"];
    publish(&dir, "mod", &say(5), &kick, &private);
    trust(&dir, &public, &["event.chat", "chat.say"]);
    assert_eq!(the(&boot(&dir)).status, Status::Pending, "player.kick is the admin's to give");
    // The admin approves it; the approval records who signed.
    let approval = approve(&dir, "mod", None).unwrap();
    assert_eq!(approval.signer.as_deref(), Some(sign::key_id(&public).as_str()));
    let mut host = boot(&dir);
    assert!(the(&host).running());
    // A new version, signed by the same key, asking for nothing more: no new approval, state handed over.
    publish(&dir, "mod", &say(3), &kick, &private);
    let before = the(&host).digest.clone();
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).running() && the(&host).digest != before, "the new version runs: {:?}", the(&host).status);
    assert!(the(&host).notice.is_none());
    let mut world = Mock::default();
    host.run(&mut world, &chat(), 0);
    assert_eq!(world.applied, vec![("mod".to_string(), Action::Say("hel".into()))]);
    // One that asks for more than was granted and vouched for waits, and the old one goes on.
    let more = ["event.chat", "chat.say", "player.kick", "rules.write"];
    publish(&dir, "mod", &say(2), &more, &private);
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).running() && the(&host).notice.as_deref().is_some_and(|n| n.contains("wait for approval")), "{:?}", the(&host).notice);
    assert_eq!(the(&host).requested.len(), 3, "still the old version");
    // The same version unsigned: the digest was never approved.
    publish(&dir, "mod", &say(3), &kick, &private);
    host.rescan(&mut Default::default(), false);
    std::fs::remove_file(dir.join("mod").join(sign::FILE)).unwrap();
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).notice.as_deref().is_some_and(|n| n.contains("wait for approval")), "{:?}", the(&host).notice);
    // Signed by another key (trusted, same grant): not the signer the approval names.
    let (other_private, other_public) = pair();
    trust(&dir, &other_public, &["event.chat", "chat.say"]);
    publish(&dir, "mod", &say(4), &kick, &other_private);
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).notice.as_deref().is_some_and(|n| n.contains("wait for approval")), "{:?}", the(&host).notice);
}

#[test]
fn files_changed_after_signing_are_caught_when_loaded_and_when_reloaded() {
    let dir = fresh_dir("sign-tamper");
    let (private, public) = pair();
    publish(&dir, "notify", &say(5), &CHAT_SAY, &private);
    trust(&dir, &public, &CHAT_SAY);
    let mut host = boot(&dir);
    assert!(the(&host).running());
    // The module is swapped for another with the old signature: at the reload, the old one goes on.
    let evil = wat::parse_str(say(1)).unwrap();
    std::fs::write(dir.join("notify/plugin.wasm"), &evil).unwrap();
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).running() && the(&host).notice.as_deref().is_some_and(|n| n.contains("signature does not hold")), "{:?}", the(&host).notice);
    let mut world = Mock::default();
    host.run(&mut world, &chat(), 0);
    assert_eq!(world.applied, vec![("notify".to_string(), Action::Say("hello".into()))], "the approved module ran, not the swapped one");
    // At a start, the tampered plugin does not run, and says why.
    let fresh = boot(&dir);
    assert_eq!(the(&fresh).status, Status::Pending);
    assert!(the(&fresh).warnings.iter().any(|w| w.contains("other files than these")), "{:?}", the(&fresh).warnings);
    // The manifest asks for more after signing: the same.
    publish(&dir, "notify", &say(5), &CHAT_SAY, &private);
    let manifest = std::fs::read_to_string(dir.join("notify/plugin.toml")).unwrap();
    std::fs::write(dir.join("notify/plugin.toml"), manifest.replace("\"chat.say\"]", "\"chat.say\", \"player.kick\"]")).unwrap();
    assert_eq!(the(&boot(&dir)).status, Status::Pending);
    // A signature file cut or garbled is the same as none.
    std::fs::write(dir.join("notify/plugin.toml"), manifest).unwrap();
    let good = std::fs::read_to_string(dir.join("notify").join(sign::FILE)).unwrap();
    std::fs::write(dir.join("notify").join(sign::FILE), &good[..good.len() / 2]).unwrap();
    assert!(matches!(the(&boot(&dir)).signed, Signed::Broken(_)) && the(&boot(&dir)).status == Status::Pending);
    std::fs::write(dir.join("notify").join(sign::FILE), good).unwrap();
    assert!(the(&boot(&dir)).running());
}

#[test]
fn a_key_taken_away_stops_its_plugins_at_the_next_look_and_cannot_come_back_by_accident() {
    let dir = fresh_dir("sign-revoke");
    let (private, public) = pair();
    publish(&dir, "notify", &say(5), &CHAT_SAY, &private);
    trust(&dir, &public, &CHAT_SAY);
    let mut host = boot(&dir);
    assert!(the(&host).running());
    let mut keys = Keys::load(&dir).unwrap();
    keys.revoke(&sign::key_id(&public)).unwrap();
    keys.save(&dir).unwrap();
    host.rescan(&mut Default::default(), false);
    assert_eq!((the(&host).status.clone(), the(&host).running()), (Status::Pending, false));
    assert!(matches!(the(&host).signed, Signed::Revoked { .. }) && the(&host).warnings.iter().any(|w| w.contains("revoked")));
    assert_eq!(host.interest, 0, "nothing of it is wanted any more");
    // Trusting it again takes --force; with it, the plugin starts again.
    let mut keys = Keys::load(&dir).unwrap();
    assert!(keys.trust(&public, "Acme", vec![], false).is_err());
    keys.trust(&public, "Acme", CHAT_SAY.iter().map(|s| s.to_string()).collect(), true).unwrap();
    keys.save(&dir).unwrap();
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).running());
    // A list of keys that does not read trusts nobody.
    std::fs::write(dir.join(sign::KEYS_FILE), "{ broken").unwrap();
    host.rescan(&mut Default::default(), false);
    assert!(!the(&host).running() && host.keys_error.is_some());
}

#[test]
fn the_admins_switch_beats_the_key_and_digest_pinning_is_still_the_default() {
    let dir = fresh_dir("sign-default");
    let (private, public) = pair();
    publish(&dir, "notify", &say(5), &CHAT_SAY, &private);
    trust(&dir, &public, &CHAT_SAY);
    let mut host = boot(&dir);
    assert!(the(&host).running());
    // Switching off a plugin that has no approval makes one that says "off".
    disable(&dir, "notify").unwrap();
    host.rescan(&mut Default::default(), false);
    assert_eq!(the(&host).status, Status::Disabled);
    assert!(disable(&dir, "no-such-plugin").is_err());
    approve(&dir, "notify", None).unwrap();
    host.rescan(&mut Default::default(), false);
    assert!(the(&host).running());
    // An unsigned plugin is as it was: approved by its digest, changed means waiting.
    let plain = fresh_dir("sign-plain");
    write_plugin(&plain, "plain", &say(5), &CHAT_SAY, SUBSCRIBE);
    approve(&plain, "plain", None).unwrap();
    assert!(the(&boot(&plain)).running());
    // A signature next to a digest approval changes nothing, good or bad.
    std::fs::write(plain.join("plain").join(sign::FILE), "garbage").unwrap();
    let host = boot(&plain);
    assert!(the(&host).running() && the(&host).warnings.iter().any(|w| w.contains("judged as if it were not signed")));
    // approved.json files from before signing (no signer) are read.
    let old = r#"{"plain": {"enabled": true, "grant": ["event.chat"], "digest": "00", "approvedAt": 1}}"#;
    std::fs::write(plain.join("approved.json"), old).unwrap();
    assert_eq!(approvals::Approvals::load(&plain).unwrap().0["plain"].signer, None);
}

#[test]
fn the_standing_of_a_plugin_is_the_approval_the_signature_and_the_keys_together() {
    use approvals::{Approval, Standing, standing};
    let key = |grant: &[&str]| sign::TrustedKey { id: "k1".into(), public_key: String::new(), name: "Acme".into(), grant: grant.iter().map(|g| g.to_string()).collect(), added_at: 0 };
    let req: Vec<String> = ["event.chat", "chat.say", "player.kick"].iter().map(|s| s.to_string()).collect();
    let approval = |enabled: bool, digest: &str, grant: &[&str], signer: Option<&str>| Approval { enabled, grant: grant.iter().map(|g| g.to_string()).collect(), digest: digest.into(), limits: Default::default(), approved_at: 0, signer: signer.map(str::to_string) };
    let names = |g: Vec<String>| g.iter().map(String::as_str).collect::<Vec<_>>().join(",");
    let none = Signed::No;
    // Digest pinning: no approval, pending; a match, running; another digest, changed.
    assert_eq!(standing(None, "d1", &req, &none).0, Standing::Pending);
    assert_eq!(standing(Some(&approval(true, "d1", &["event.chat"], None)), "d1", &req, &none), (Standing::Running, vec!["event.chat".to_string()]));
    assert_eq!(standing(Some(&approval(true, "d0", &["event.chat"], None)), "d1", &req, &none).0, Standing::Changed);
    assert_eq!(standing(Some(&approval(false, "d1", &[], None)), "d1", &req, &none).0, Standing::Disabled);
    // A key that vouches for everything: running with no approval; a switch-off still wins.
    let all = Signed::Trusted(key(&["event.chat", "chat.say", "player.kick", "rules.write"]));
    assert_eq!(standing(None, "d1", &req, &all), (Standing::Running, req.clone()));
    assert_eq!(standing(Some(&approval(false, "d1", &[], None)), "d1", &req, &all).0, Standing::Disabled);
    // A key that vouches for part: pending, and the approval then covers the rest and the updates.
    let part = Signed::Trusted(key(&["event.chat", "chat.say"]));
    assert_eq!(standing(None, "d1", &req, &part).0, Standing::Pending);
    let (s, g) = standing(Some(&approval(true, "d0", &["player.kick"], Some("k1"))), "d1", &req, &part);
    assert_eq!((s, names(g)), (Standing::Running, "event.chat,chat.say,player.kick".to_string()));
    assert_eq!(standing(Some(&approval(true, "d0", &["player.kick"], Some("k2"))), "d1", &req, &part).0, Standing::Changed, "another signer");
    assert_eq!(standing(Some(&approval(true, "d0", &["player.kick"], None)), "d1", &req, &part).0, Standing::Changed, "approved by digest only");
    let more: Vec<String> = req.iter().cloned().chain(["rules.write".to_string()]).collect();
    assert_eq!(standing(Some(&approval(true, "d0", &["player.kick"], Some("k1"))), "d1", &more, &part).0, Standing::Changed, "asks for more than was granted");
    // Not a trusted signature: no help at all.
    for s in [Signed::Unknown { id: "k1".into() }, Signed::Revoked { id: "k1".into() }, Signed::Broken("x".into())] {
        assert_eq!(standing(None, "d1", &req, &s).0, Standing::Pending);
        assert_eq!(standing(Some(&approval(true, "d0", &["player.kick"], Some("k1"))), "d1", &req, &s).0, Standing::Changed);
    }
}

#[test]
fn the_signed_example_is_current() {
    // The example key is public on purpose (examples/plugins/README.md); an Ed25519 signature is the
    // same bytes every time, so the one in the repository must be the one the files make today.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins");
    let key = std::fs::read_to_string(root.join("example-publisher.key")).unwrap();
    let public = sign::parse_public(&std::fs::read_to_string(root.join("example-publisher.pub")).unwrap()).unwrap();
    let folder = root.join("discord-notify");
    let digest = read_folder(&folder).digest;
    let committed = std::fs::read_to_string(folder.join(sign::FILE)).unwrap();
    assert_eq!(sign::sign(&key, &digest).unwrap(), committed, "plugin.sig is not what the files make: scripts/build-plugin-examples, then scripts/sign-plugin-examples");
    let mut keys = Keys::default();
    assert_eq!(sign::check(&folder, &digest, &keys), Signed::Unknown { id: sign::key_id(&public) }, "nobody trusts the example key by default");
    keys.trust(&public, "Examples", ["event.player", "event.chat", "event.tick", "chat.say"].iter().map(|s| s.to_string()).collect(), false).unwrap();
    // Trusted for all but net.http: the plugin waits for the admin to approve that one, and then runs.
    let dir = fresh_dir("sign-example");
    let to = dir.join("discord-notify");
    std::fs::create_dir_all(&to).unwrap();
    for f in ["plugin.toml", "plugin.wasm", sign::FILE] {
        std::fs::copy(folder.join(f), to.join(f)).unwrap();
    }
    keys.save(&dir).unwrap();
    assert_eq!(the(&boot(&dir)).status, Status::Pending);
    let approval = approve(&dir, "discord-notify", None).unwrap();
    assert_eq!(approval.signer, Some(sign::key_id(&public)));
    let host = boot(&dir);
    assert!(the(&host).running(), "{:?}", the(&host).status);
    assert_eq!(the(&host).grant.names().len(), 5);
}

/// The example publisher's private key is in the repository: a plugin anyone signs with it, under a
/// key an admin trusted by following the README, runs without approval, and the listing says why that is no trust.
#[test]
fn a_plugin_signed_by_the_published_example_key_carries_a_warning() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins");
    let private = std::fs::read_to_string(root.join("example-publisher.key")).unwrap();
    let public = sign::parse_public(&std::fs::read_to_string(root.join("example-publisher.pub")).unwrap()).unwrap();
    let dir = fresh_dir("sign-published");
    publish(&dir, "p", &say(5), &CHAT_SAY, &private);
    trust(&dir, &public, &["event.chat", "chat.say"]);
    let host = boot(&dir);
    assert!(the(&host).running(), "it runs (the key vouches for what it asks)");
    assert!(the(&host).warnings.iter().any(|w| w.contains("published") && w.contains("anybody can sign")), "{:?}", the(&host).warnings);
    // A key nobody published carries none.
    let (private, public) = pair();
    let dir = fresh_dir("sign-own");
    publish(&dir, "p", &say(5), &CHAT_SAY, &private);
    trust(&dir, &public, &["event.chat", "chat.say"]);
    assert!(the(&boot(&dir)).warnings.is_empty());
}
