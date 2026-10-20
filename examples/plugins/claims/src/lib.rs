//! claims: players claim ground, and others cannot build or take things apart inside it.
//!
//!   !claim [radius]     claim a sphere around where you stand
//!   !unclaim            give up the claim you stand in
//!   !trust <name>       let another player (online now) build and take apart in your claims
//!   !untrust <name>     take that back
//!   !claims             list your claims
//!
//! The protection is a veto: before the game runs a client's request to build or to take an object
//! apart, the plugin looks at where it would happen. Inside somebody else's claim, it drops the
//! request and tells the player why. The owner, the players the owner trusts and the server's
//! operators pass.
//!
//! A claim is a sphere (centre and radius) and belongs to a `player_id`, which is the same on every
//! visit; the connection number (`client`) is only this visit's. What is kept is bounded: 400 claims,
//! 8 trusted players on each, names cut to 32 characters. Nothing here panics on what a player types
//! or a client sends: an argument that does not parse is answered with the usage, and an RPC that
//! does not parse is let through (a bug in this plugin must never stop the game).

use pcds_plugin_api::json;
use pcds_plugin_api::*;

/// The version of the saved bytes. Version 1: a count, then each claim (see `Claims::save`).
const STATE_VERSION: u32 = 1;
/// Claims on the whole server. At 8 trusted players of 32 characters each the state is under 150 KB.
const MAX_CLAIMS_TOTAL: usize = 400;
const MAX_TRUSTED: usize = 8;
const NAME_CHARS: usize = 32;
/// The most characters of a player name typed after `!trust`.
const TYPED_NAME_CHARS: usize = 64;
/// A claim's radius, however the state came: at most this many metres (a hostile state is cut).
const HARD_MAX_RADIUS: f32 = 1000.0;
/// A player is told at most once in this many milliseconds of game time that a request was refused.
const TELL_GAP_MS: i64 = 3000;
/// How many players the "told lately" list remembers.
const TELL_MEMORY: usize = 32;
/// The arguments of the build request are about 100 bytes; more is not the request we know.
const MAX_ARGS_JSON: usize = 1024;
/// The longest chat line the plugin builds before it starts another (the chat takes 500).
const LINE_CHARS: usize = 380;
/// `!claims` names this many trusted players of a claim, and counts the rest.
const LISTED_NAMES: usize = 3;
/// `!claims` sends at most this many lines (20 claims of the longest kind need about 8).
const MAX_LIST_LINES: usize = 12;

const RPC_DECONSTRUCT: &str = "GroupNetworkBase.DeconstructServerRpc";
const RPC_CREATE: &str = "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc";

#[derive(Debug, Clone, PartialEq)]
struct Trusted {
    id: u64,
    name: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Claim {
    /// The owner's `player_id`.
    owner: u64,
    /// The owner's name when they last used a command (to tell others whose ground it is).
    owner_name: String,
    center: [f32; 3],
    radius: f32,
    trusted: Vec<Trusted>,
}

/// The admin's settings, clamped to what the manifest allows even if the host were to hand over more.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Settings {
    min_radius: u32,
    max_radius: u32,
    default_radius: u32,
    max_claims: usize,
}

impl Settings {
    fn read(host: &dyn Host) -> Settings {
        let int = |key: &str, default: i64, least: i64, most: i64| host.config_i64(key).unwrap_or(default).clamp(least, most);
        let min_radius = int("min_radius", 5, 1, 100) as u32;
        let max_radius = (int("max_radius", 50, 5, 200) as u32).max(min_radius);
        let default_radius = (int("default_radius", 20, 1, 200) as u32).clamp(min_radius, max_radius);
        Settings { min_radius, max_radius, default_radius, max_claims: int("max_claims", 3, 1, 20) as usize }
    }
}

struct Claims {
    settings: Settings,
    claims: Vec<Claim>,
    /// (client, game time) of the last "you cannot build here" told, to tell a player who holds the
    /// build button once and not thirty times a second.
    told: Vec<(u64, i64)>,
    /// Whether the log already said that an RPC could not be read, or an object not found.
    logged_unreadable: bool,
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    x * x + y * y + z * z
}

fn finite(p: [f32; 3]) -> bool {
    p.iter().all(|v| v.is_finite())
}

/// A name from outside, safe to repeat in the chat: no control characters, cut.
fn clean(text: &str, most: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(most).collect()
}

fn place(p: [f32; 3]) -> String {
    format!("({}, {}, {})", p[0] as i32, p[1] as i32, p[2] as i32)
}

/// A whole number of 1 to 6 digits, nothing else (`+5`, `5.0`, `0x10`, `-3` and other scripts' digits are no).
fn parse_radius(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 6 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Claim {
    fn contains(&self, p: [f32; 3]) -> bool {
        dist2(self.center, p) <= self.radius * self.radius
    }

    fn lets_in(&self, who: &PlayerInfo) -> bool {
        who.operator || self.owner == who.player_id || self.trusted.iter().any(|t| t.id == who.player_id)
    }
}

impl Claims {
    fn tell(&self, host: &dyn Host, client: u64, text: &str) {
        let _ = host.tell(client, text);
    }

    /// The player asking, or `None`: they have left, or the server will not say (and then they are told).
    fn asker(&self, host: &dyn Host, client: u64) -> Option<PlayerInfo> {
        match host.player(client) {
            Ok(me) => Some(me),
            Err(Error::NotFound) => None,
            Err(_) => {
                self.tell(host, client, "I cannot see who is online right now, ask an admin.");
                None
            }
        }
    }

    /// The owner's name may have changed since they claimed.
    fn refresh_name(&mut self, me: &PlayerInfo) {
        let name = clean(&me.name, NAME_CHARS);
        for c in self.claims.iter_mut().filter(|c| c.owner == me.player_id && c.owner_name != name) {
            c.owner_name = name.clone();
        }
    }

    fn mine(&self, id: u64) -> impl Iterator<Item = &Claim> {
        self.claims.iter().filter(move |c| c.owner == id)
    }

    // ---- The commands.

    fn claim(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let Some(me) = self.asker(host, client) else { return };
        self.refresh_name(&me);
        let s = self.settings;
        let radius = match args.first() {
            None => s.default_radius,
            Some(a) => match parse_radius(a) {
                Some(r) => r,
                None => return self.tell(host, client, &format!("Use: !claim [radius]. The radius is a whole number of metres, {} to {}; {} if you give none.", s.min_radius, s.max_radius, s.default_radius)),
            },
        };
        if !(s.min_radius..=s.max_radius).contains(&radius) {
            return self.tell(host, client, &format!("The radius must be from {} to {} metres.", s.min_radius, s.max_radius));
        }
        if !finite(me.position) {
            return self.tell(host, client, "I cannot tell where you are, try again in a moment.");
        }
        let have = self.mine(me.player_id).count();
        if have >= s.max_claims {
            return self.tell(host, client, &format!("You have {have} claim{}, the most is {}. Stand in one and use !unclaim to give it up.", if have == 1 { "" } else { "s" }, s.max_claims));
        }
        if self.claims.len() >= MAX_CLAIMS_TOTAL {
            return self.tell(host, client, "The server has no room for more claims, tell an admin.");
        }
        let r = radius as f32;
        if let Some(other) = self.claims.iter().find(|c| c.owner != me.player_id && dist2(c.center, me.position) < (c.radius + r) * (c.radius + r)) {
            return self.tell(host, client, &format!("That overlaps the claim of {}. Move away from it or ask for a smaller radius.", other.owner_name));
        }
        // A new claim starts with the players the owner trusts already.
        let trusted = self.mine(me.player_id).next().map(|c| c.trusted.clone()).unwrap_or_default();
        self.claims.push(Claim { owner: me.player_id, owner_name: clean(&me.name, NAME_CHARS), center: me.position, radius: r, trusted });
        self.tell(host, client, &format!("Claimed radius {radius} around {}. Only you and the players you !trust can build or take apart here.", place(me.position)));
    }

    fn unclaim(&mut self, host: &dyn Host, client: u64) {
        let Some(me) = self.asker(host, client) else { return };
        self.refresh_name(&me);
        // The claim whose centre is nearest among those that hold the player: they may overlap one's own.
        let inside = |c: &&Claim| c.contains(me.position);
        let nearest = self.claims.iter().enumerate().filter(|(_, c)| inside(c)).min_by(|a, b| dist2(a.1.center, me.position).total_cmp(&dist2(b.1.center, me.position))).map(|(i, c)| (i, c.owner, c.owner_name.clone()));
        let Some((i, owner, owner_name)) = nearest else {
            return self.tell(host, client, "You are not standing in a claim.");
        };
        if owner != me.player_id && !me.operator {
            return self.tell(host, client, &format!("This ground is claimed by {owner_name}, not by you."));
        }
        let gone = self.claims.remove(i);
        let whose = if owner == me.player_id { "your claim".to_string() } else { format!("the claim of {owner_name}") };
        self.tell(host, client, &format!("Removed {whose} at {}, radius {}.", place(gone.center), gone.radius as u32));
    }

    fn trust(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let Some(me) = self.asker(host, client) else { return };
        let Ok(all) = host.players() else { return self.tell(host, client, "I cannot see who is online right now, ask an admin.") };
        self.refresh_name(&me);
        let Some(wanted) = typed_name(args) else { return self.tell(host, client, "Use: !trust <player name>") };
        if self.mine(me.player_id).next().is_none() {
            return self.tell(host, client, "You have no claims yet: stand where you want one and use !claim.");
        }
        let shown = clean(&wanted, NAME_CHARS);
        let lower = wanted.to_lowercase();
        let mut found = all.iter().filter(|p| p.name.to_lowercase() == lower);
        let (Some(target), None) = (found.next(), found.next()) else {
            let several = all.iter().filter(|p| p.name.to_lowercase() == lower).count() > 1;
            return self.tell(host, client, &if several { format!("More than one player is called {shown}.") } else { format!("No player called {shown} is online.") });
        };
        if target.player_id == me.player_id {
            return self.tell(host, client, "Your own claims are always yours.");
        }
        let (mut added, mut full) = (false, false);
        for c in self.claims.iter_mut().filter(|c| c.owner == me.player_id) {
            if c.trusted.iter().any(|t| t.id == target.player_id) {
                continue;
            }
            if c.trusted.len() >= MAX_TRUSTED {
                full = true;
                continue;
            }
            c.trusted.push(Trusted { id: target.player_id, name: clean(&target.name, NAME_CHARS) });
            added = true;
        }
        let name = clean(&target.name, NAME_CHARS);
        let text = match (added, full) {
            (true, false) => format!("{name} can now build and take apart in your claims."),
            (true, true) => format!("{name} was added where there was room; some of your claims already trust {MAX_TRUSTED} players."),
            (false, true) => format!("Your claims already trust {MAX_TRUSTED} players: use !untrust first."),
            (false, false) => format!("{name} is already trusted."),
        };
        self.tell(host, client, &text);
    }

    fn untrust(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let Some(me) = self.asker(host, client) else { return };
        self.refresh_name(&me);
        let Some(wanted) = typed_name(args) else { return self.tell(host, client, "Use: !untrust <player name>") };
        let lower = wanted.to_lowercase();
        // The name is looked for among those trusted, so a player who has gone offline can be untrusted too.
        let mut removed: Option<String> = None;
        for c in self.claims.iter_mut().filter(|c| c.owner == me.player_id) {
            c.trusted.retain(|t| {
                let hit = t.name.to_lowercase() == lower;
                if hit {
                    removed.get_or_insert_with(|| t.name.clone());
                }
                !hit
            });
        }
        let text = match removed {
            Some(name) => format!("{name} can no longer build or take apart in your claims."),
            None => format!("{} is not trusted by you.", clean(&wanted, NAME_CHARS)),
        };
        self.tell(host, client, &text);
    }

    fn list(&mut self, host: &dyn Host, client: u64) {
        let Some(me) = self.asker(host, client) else { return };
        self.refresh_name(&me);
        let mine: Vec<&Claim> = self.mine(me.player_id).collect();
        if mine.is_empty() {
            return self.tell(host, client, "You have no claims. Stand where you want one and use !claim.");
        }
        let mut line = format!("Your claims ({} of {}):", mine.len(), self.settings.max_claims);
        let mut sent = 0;
        // A line is at most LINE_CHARS long; at most 20 claims make about 8 lines, under the limit of 16 actions.
        for (i, c) in mine.iter().enumerate() {
            let names = c.trusted.iter().take(LISTED_NAMES).map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ");
            let more = c.trusted.len().saturating_sub(LISTED_NAMES);
            let trusted = match (c.trusted.is_empty(), more) {
                (true, _) => String::new(),
                (false, 0) => format!(", trusted: {names}"),
                (false, n) => format!(", trusted: {names} and {n} more"),
            };
            let item = format!(" #{} at {} radius {}{trusted};", i + 1, place(c.center), c.radius as u32);
            if line.chars().count() + item.chars().count() > LINE_CHARS {
                self.tell(host, client, &line);
                sent += 1;
                line = String::new();
                if sent >= MAX_LIST_LINES {
                    return;
                }
            }
            line.push_str(&item);
        }
        self.tell(host, client, line.trim_start());
    }

    // ---- The protection.

    /// `rpc_before` for the two requests: the veto.
    fn guard(&mut self, host: &dyn Host, client: u64, rpc: &str, object: u64, args_json: &str) -> Verdict {
        // The cheap test first: with no claims there is nothing to protect.
        if self.claims.is_empty() {
            return Verdict::Continue;
        }
        let (position, doing) = match rpc {
            RPC_DECONSTRUCT => match host.object(ObjectRef::Net(object)) {
                Ok(Some(o)) => (o.position, "take this apart"),
                Ok(None) => return Verdict::Continue,
                Err(e) => return self.unreadable(host, &format!("cannot look up object {object}: {e:?}")),
            },
            RPC_CREATE => match build_position(args_json) {
                Some(p) => (p, "build"),
                None => return self.unreadable(host, "cannot read the position of a build request"),
            },
            _ => return Verdict::Continue,
        };
        let Some(claim) = self.claims.iter().find(|c| c.contains(position)) else { return Verdict::Continue };
        // Who is it? Only now, in a claim, is the player worth asking for (one player, not the roster).
        let Ok(who) = host.player(client) else { return Verdict::Continue };
        if claim.lets_in(&who) {
            return Verdict::Continue;
        }
        let text = format!("This ground is claimed by {}. You cannot {doing} here.", claim.owner_name);
        // There is no clock in an RPC event: the game time comes from the world.
        let now = host.world().map(|w| w.game_ms).unwrap_or(0);
        if self.may_tell(client, now) {
            self.tell(host, client, &text);
        }
        Verdict::Veto
    }

    /// Lets the RPC through (never block play because of a plugin bug) and logs it, once.
    fn unreadable(&mut self, host: &dyn Host, why: &str) -> Verdict {
        if !self.logged_unreadable {
            self.logged_unreadable = true;
            host.log(Level::Warn, &format!("{why}: letting it through (this is logged once)"));
        }
        Verdict::Continue
    }

    /// Whether `client` may be told now, and notes that they were.
    fn may_tell(&mut self, client: u64, now: i64) -> bool {
        if let Some(entry) = self.told.iter_mut().find(|(c, _)| *c == client) {
            if now - entry.1 < TELL_GAP_MS && now >= entry.1 {
                return false;
            }
            entry.1 = now;
            return true;
        }
        if self.told.len() >= TELL_MEMORY {
            self.told.remove(0);
        }
        self.told.push((client, now));
        true
    }
}

/// The name typed after `!trust`: the arguments, joined (a name may have spaces). `None` if empty or too long.
fn typed_name(args: &[&str]) -> Option<String> {
    let joined = args.join(" ");
    let joined = joined.trim();
    (!joined.is_empty() && joined.chars().count() <= TYPED_NAME_CHARS).then(|| joined.to_string())
}

/// The position in the arguments of the build request: `[-12345,[1.5,2.0,-3.25],[0,0.5,0,1],true,...]`.
///
/// This runs on every build request while any claim exists, so it takes a short way first: it cuts the
/// second item out of the text and reads three numbers. Anything that is not exactly that goes to the
/// real JSON reader, which also decides what is not JSON at all.
fn build_position(args_json: &str) -> Option<[f32; 3]> {
    if args_json.len() > MAX_ARGS_JSON {
        return None;
    }
    quick_position(args_json).or_else(|| json::parse(args_json).ok()?.index(1)?.vec3())
}

fn quick_position(args_json: &str) -> Option<[f32; 3]> {
    let after_id = args_json.strip_prefix('[')?.split_once(',')?.1;
    let (coords, _) = after_id.strip_prefix('[')?.split_once(']')?;
    let mut numbers = coords.split(',').map(|n| n.parse::<f32>().ok().filter(|v| v.is_finite()));
    let p = [numbers.next()??, numbers.next()??, numbers.next()??];
    numbers.next().is_none().then_some(p)
}

impl Plugin for Claims {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let mut me = Claims { settings: Settings::read(host), claims: Vec::new(), told: Vec::new(), logged_unreadable: false };
        let Some(state) = state else { return me };
        match state.version {
            1 => {
                let (claims, whole) = read_claims(state.bytes);
                me.claims = claims;
                if !whole {
                    host.log(Level::Warn, &format!("the saved claims end too soon or are damaged: kept the {} that could be read", me.claims.len()));
                }
            }
            v => host.log(Level::Warn, &format!("state version {v} is newer than this plugin knows: starting with no claims")),
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::RpcBefore { client, rpc, object, args_json } => return self.guard(host, *client, rpc, *object, args_json),
            Event::Command { client, command, args, .. } => match *command {
                "claim" => self.claim(host, *client, args),
                "unclaim" => self.unclaim(host, *client),
                "trust" => self.trust(host, *client, args),
                "untrust" => self.untrust(host, *client, args),
                "claims" => self.list(host, *client),
                _ => {}
            },
            _ => {}
        }
        Verdict::Continue
    }

    /// Version 1: `u32` count; per claim `u64` owner, name, 3 x `f32` centre, `f32` radius, `u32`
    /// number trusted, and per trusted `u64` id and name. Always written, even with no claims, so that
    /// giving up the last claim is kept too.
    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.claims.len() as u32);
        for c in &self.claims {
            w.u64(c.owner).str(&c.owner_name).f32(c.center[0]).f32(c.center[1]).f32(c.center[2]).f32(c.radius).u32(c.trusted.len() as u32);
            for t in &c.trusted {
                w.u64(t.id).str(&t.name);
            }
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

/// The claims in version 1 bytes, and whether all of the bytes were good. A claim that is not sane (a
/// radius of 0 or NaN, a centre at infinity), more claims than the limit and more trusted players than
/// the limit are dropped; the rest is kept.
fn read_claims(bytes: &[u8]) -> (Vec<Claim>, bool) {
    let mut r = Reader::new(bytes);
    let mut claims = Vec::new();
    let Ok(count) = r.u32() else { return (claims, false) };
    let mut whole = true;
    for _ in 0..count {
        match read_claim(&mut r) {
            Ok(Some(c)) if claims.len() < MAX_CLAIMS_TOTAL => claims.push(c),
            Ok(_) => whole = false,
            Err(_) => return (claims, false),
        }
    }
    (claims, whole)
}

fn read_claim(r: &mut Reader<'_>) -> Result<Option<Claim>, DecodeError> {
    let (owner, owner_name) = (r.u64()?, clean(r.str()?, NAME_CHARS));
    let center = [r.f32()?, r.f32()?, r.f32()?];
    let radius = r.f32()?;
    let n = r.u32()? as usize;
    // A trusted player takes at least 12 bytes: a count the bytes cannot hold is a short state.
    if n > r.rest().len() / 12 {
        return Err(DecodeError::Short);
    }
    let mut trusted = Vec::new();
    for _ in 0..n {
        let (id, name) = (r.u64()?, clean(r.str()?, NAME_CHARS));
        if trusted.len() < MAX_TRUSTED {
            trusted.push(Trusted { id, name });
        }
    }
    let sane = finite(center) && radius.is_finite() && radius > 0.0 && radius <= HARD_MAX_RADIUS;
    Ok(sane.then_some(Claim { owner, owner_name, center, radius, trusted }))
}

export!(Claims);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    const ANN: u64 = 1;
    const BO: u64 = 2;
    const CY: u64 = 3;

    fn id(client: u64) -> u64 {
        client + 1000
    }

    fn player(client: u64, name: &str, position: [f32; 3], operator: bool) -> PlayerInfo {
        PlayerInfo { client, name: name.into(), steam_id: String::new(), position, gauges: [100.0; 4], player_id: id(client), operator }
    }

    /// Ann, Bo and Cy online, all at the origin.
    fn world() -> Harness<Claims> {
        let mock = MockHost::new();
        for (c, n) in [(ANN, "Ann"), (BO, "Bo"), (CY, "Cy")] {
            mock.add_player(player(c, n, [0.0; 3], false));
        }
        declared(Harness::start(mock))
    }

    /// The manifest's events and commands.
    fn declared(mut t: Harness<Claims>) -> Harness<Claims> {
        for (name, least, most) in [("claim", 0, 1), ("unclaim", 0, 0), ("trust", 1, 8), ("untrust", 1, 8), ("claims", 0, 0)] {
            t.declare_command(name, least, most, false);
        }
        t.subscribe(&[Kind::RpcBefore])
    }

    /// Moves a player.
    fn go(t: &Harness<Claims>, client: u64, position: [f32; 3]) {
        t.mock().set_position(client, position);
    }

    fn last_told(t: &Harness<Claims>) -> String {
        t.mock().told().last().map(|(_, m)| m.clone()).unwrap_or_default()
    }

    fn net_object(t: &mut Harness<Claims>, id: i32, net: u64, position: [f32; 3]) {
        t.place_net(id, "Container1", position, net, 0);
    }

    const BUILD: &str = "[-12345,[1.5,2.0,-3.25],[0.0,0.5,0.0,1.0],true,false,true,false]";

    fn build_at(x: f32) -> String {
        format!("[-12345,[{x},2.0,-3.25],[0.0,0.5,0.0,1.0],true,false,true,false]")
    }

    // ---- Claiming.

    #[test]
    fn a_claim_is_made_around_the_player_and_listed() {
        let mut t = world();
        go(&t, ANN, [100.0, 5.0, -40.0]);
        t.command(ANN, "claim", &["30"]);
        assert_eq!(last_told(&t), "Claimed radius 30 around (100, 5, -40). Only you and the players you !trust can build or take apart here.");
        // No radius: the default of 20.
        go(&t, ANN, [500.0, 0.0, 0.0]);
        t.command(ANN, "claim", &[]);
        assert!(last_told(&t).starts_with("Claimed radius 20 around (500, 0, 0)."), "{}", last_told(&t));
        t.command(ANN, "claims", &[]);
        assert_eq!(last_told(&t), "Your claims (2 of 3): #1 at (100, 5, -40) radius 30; #2 at (500, 0, 0) radius 20;");
        t.command(BO, "claims", &[]);
        assert_eq!(last_told(&t), "You have no claims. Stand where you want one and use !claim.");
    }

    #[test]
    fn the_radius_is_checked_and_garbage_gets_the_usage() {
        let mut t = world();
        for bad in ["", "abc", "-5", "+5", "5.5", "1e3", "0x10", "٣٠", "99999999999999999999", "30m", " ", "\u{0}", &"9".repeat(10_000)] {
            t.command(ANN, "claim", &[bad]);
            assert!(last_told(&t).starts_with("Use: !claim [radius]."), "{bad:?} -> {}", last_told(&t));
        }
        for (r, ok) in [("4", false), ("5", true), ("50", true), ("51", false), ("0", false), ("200", false)] {
            let mut t = world();
            t.command(ANN, "claim", &[r]);
            assert_eq!(last_told(&t).starts_with("Claimed"), ok, "{r}: {}", last_told(&t));
            if !ok {
                assert_eq!(last_told(&t), "The radius must be from 5 to 50 metres.");
            }
        }
        // More words than the manifest allows: the host answers, the plugin is not called.
        t.command(ANN, "claim", &["10", "20", "30"]);
        assert_eq!(last_told(&t), "Use: !claim");
        assert!(t.plugin().claims.is_empty());
    }

    #[test]
    fn the_settings_move_the_limits_and_are_kept_in_their_bounds() {
        let mock = MockHost::new().with_setting("max_radius", "100").with_setting("min_radius", "10").with_setting("default_radius", "5").with_setting("max_claims", "1");
        mock.add_player(player(ANN, "Ann", [0.0; 3], false));
        let mut t = Harness::<Claims>::start(mock);
        t.command(ANN, "claim", &[]);
        assert!(last_told(&t).starts_with("Claimed radius 10 "), "the default is kept between min and max: {}", last_told(&t));
        t.command(ANN, "claim", &["100"]);
        assert!(last_told(&t).starts_with("You have 1 claim, the most is 1."), "{}", last_told(&t));
        // Nonsense in the settings falls back to the defaults, or to the manifest's bounds.
        let mock = MockHost::new().with_setting("max_radius", "lots").with_setting("min_radius", "-4").with_setting("max_claims", "999");
        let t = Harness::<Claims>::start(mock);
        assert_eq!(t.plugin().settings, Settings { min_radius: 1, max_radius: 50, default_radius: 20, max_claims: 20 });
        // A minimum above the maximum does not make an empty range.
        let mock = MockHost::new().with_setting("max_radius", "5").with_setting("min_radius", "30");
        let t = Harness::<Claims>::start(mock);
        assert_eq!(t.plugin().settings.max_radius, 30);
    }

    #[test]
    fn at_most_max_claims_and_not_over_somebody_elses() {
        let mut t = world();
        for (i, x) in [0.0, 1000.0, 2000.0].into_iter().enumerate() {
            go(&t, ANN, [x, 0.0, 0.0]);
            t.command(ANN, "claim", &["10"]);
            assert!(last_told(&t).starts_with("Claimed"), "claim {i}");
        }
        go(&t, ANN, [3000.0, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        assert_eq!(last_told(&t), "You have 3 claims, the most is 3. Stand in one and use !unclaim to give it up.");
        // Bo next to Ann's claim at x=0: radius 10 + 10 means the centres must be 20 metres apart or more.
        go(&t, BO, [19.9, 0.0, 0.0]);
        t.command(BO, "claim", &["10"]);
        assert_eq!(last_told(&t), "That overlaps the claim of Ann. Move away from it or ask for a smaller radius.");
        go(&t, BO, [20.0, 0.0, 0.0]);
        t.command(BO, "claim", &["10"]);
        assert!(last_told(&t).starts_with("Claimed radius 10"), "touching is not overlapping: {}", last_told(&t));
        // One's own claims may overlap.
        go(&t, BO, [25.0, 0.0, 0.0]);
        t.command(BO, "claim", &["10"]);
        assert!(last_told(&t).starts_with("Claimed"), "{}", last_told(&t));
    }

    #[test]
    fn the_server_wide_limit_and_a_player_who_left_or_is_not_seen() {
        let mut t = world();
        for i in 0..MAX_CLAIMS_TOTAL {
            t.plugin_mut().claims.push(Claim { owner: 9999, owner_name: "x".into(), center: [10_000.0 + 100.0 * i as f32, 0.0, 0.0], radius: 5.0, trusted: Vec::new() });
        }
        t.command(ANN, "claim", &["10"]);
        assert_eq!(last_told(&t), "The server has no room for more claims, tell an admin.");
        // A client that is gone: nothing is told, nothing is claimed.
        let mut t = world();
        t.mock().remove_player(ANN);
        t.command(ANN, "claim", &["10"]);
        t.command(ANN, "trust", &["Bo"]);
        t.command(ANN, "unclaim", &[]);
        assert!(t.mock().told().is_empty() && t.plugin().claims.is_empty());
        // A position that is not a number.
        let mut t = world();
        go(&t, ANN, [f32::NAN, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        assert!(last_told(&t).starts_with("I cannot tell where you are"));
        assert!(t.plugin().claims.is_empty());
    }

    #[test]
    fn unclaim_gives_up_the_claim_you_stand_in_if_it_is_yours() {
        let mut t = world();
        go(&t, ANN, [0.0, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        go(&t, ANN, [3.0, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        // Standing in both: the one whose centre is nearest goes.
        t.command(ANN, "unclaim", &[]);
        assert_eq!(last_told(&t), "Removed your claim at (3, 0, 0), radius 10.");
        assert_eq!(t.plugin().claims.len(), 1);
        go(&t, ANN, [500.0, 0.0, 0.0]);
        t.command(ANN, "unclaim", &[]);
        assert_eq!(last_told(&t), "You are not standing in a claim.");
        // Somebody else's: refused, unless you are an operator.
        go(&t, BO, [1.0, 0.0, 0.0]);
        t.command(BO, "unclaim", &[]);
        assert_eq!(last_told(&t), "This ground is claimed by Ann, not by you.");
        assert_eq!(t.plugin().claims.len(), 1);
        go(&t, CY, [1.0, 0.0, 0.0]);
        t.mock().set_operator(CY, true);
        t.command(CY, "unclaim", &[]);
        assert_eq!(last_told(&t), "Removed the claim of Ann at (0, 0, 0), radius 10.");
        assert!(t.plugin().claims.is_empty());
    }

    // ---- Trust.

    #[test]
    fn trust_and_untrust_work_by_name_and_apply_to_every_claim_of_the_owner() {
        let mut t = world();
        t.command(ANN, "trust", &["Bo"]);
        assert_eq!(last_told(&t), "You have no claims yet: stand where you want one and use !claim.");
        t.command(ANN, "claim", &["10"]);
        go(&t, ANN, [1000.0, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        t.command(ANN, "trust", &["bo"]);
        assert_eq!(last_told(&t), "Bo can now build and take apart in your claims.");
        assert!(t.plugin().claims.iter().all(|c| c.trusted.len() == 1));
        t.command(ANN, "trust", &["BO"]);
        assert_eq!(last_told(&t), "Bo is already trusted.");
        // A claim made later starts with the same people.
        go(&t, ANN, [2000.0, 0.0, 0.0]);
        t.command(ANN, "claim", &["10"]);
        assert!(t.plugin().claims.iter().all(|c| c.trusted.len() == 1));
        t.command(ANN, "claims", &[]);
        assert!(last_told(&t).contains("radius 10, trusted: Bo;"), "{}", last_told(&t));
        t.command(ANN, "trust", &["Ann"]);
        assert_eq!(last_told(&t), "Your own claims are always yours.");
        t.command(ANN, "trust", &["Nobody"]);
        assert_eq!(last_told(&t), "No player called Nobody is online.");
        // Bo goes; Ann can still take the trust back, by the name.
        t.mock().remove_player(BO);
        t.command(ANN, "untrust", &["Bo"]);
        assert_eq!(last_told(&t), "Bo can no longer build or take apart in your claims.");
        assert!(t.plugin().claims.iter().all(|c| c.trusted.is_empty()));
        t.command(ANN, "untrust", &["Bo"]);
        assert_eq!(last_told(&t), "Bo is not trusted by you.");
    }

    #[test]
    fn names_with_spaces_two_players_of_one_name_and_hostile_names() {
        let mut t = world();
        t.command(ANN, "claim", &["10"]);
        t.mock().add_player(player(10, "Per Arne", [0.0; 3], false));
        t.command(ANN, "trust", &["Per", "Arne"]);
        assert_eq!(last_told(&t), "Per Arne can now build and take apart in your claims.");
        t.mock().add_player(player(11, "Twin", [0.0; 3], false));
        t.mock().add_player(player(12, "twin", [0.0; 3], false));
        t.command(ANN, "trust", &["Twin"]);
        assert_eq!(last_told(&t), "More than one player is called Twin.");
        // Empty, too long, and control characters: answered, never echoed whole.
        for args in [vec![], vec![""], vec!["   "], vec![&"x".repeat(5000)[..]], vec!["a\nb", "\u{7}"]] {
            t.command(ANN, "trust", &args);
            t.command(ANN, "untrust", &args);
        }
        assert!(t.mock().told().iter().all(|(_, m)| m.chars().count() < 200 && !m.contains('\n') && !m.contains('\u{7}')), "{:?}", t.mock().told());
        // At most 8 trusted players.
        for i in 0..10u64 {
            t.mock().add_player(player(100 + i, &format!("P{i}"), [0.0; 3], false));
            t.command(ANN, "trust", &[&format!("P{i}")]);
        }
        assert!(t.plugin().claims.iter().all(|c| c.trusted.len() == MAX_TRUSTED));
        assert_eq!(last_told(&t), "Your claims already trust 8 players: use !untrust first.");
    }

    // ---- The protection.

    /// Ann has a claim of radius 20 at the origin and trusts Bo; Cy is a stranger.
    fn claimed_world() -> Harness<Claims> {
        let mut t = world();
        t.command(ANN, "claim", &["20"]);
        t.command(ANN, "trust", &["Bo"]);
        t.mock().clear();
        t
    }

    #[test]
    fn a_stranger_cannot_take_apart_or_build_inside_a_claim_and_is_told_why() {
        let mut t = claimed_world();
        net_object(&mut t, 1, 777, [5.0, 0.0, 0.0]);
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 777, "[]"), Verdict::Veto);
        assert_eq!(t.mock().told(), [(CY, "This ground is claimed by Ann. You cannot take this apart here.".to_string())]);
        t.mock().clear();
        // A player is told once in three seconds (the next test), so let the time pass.
        t.advance_ms(3000);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Veto);
        assert_eq!(t.mock().told(), [(CY, "This ground is claimed by Ann. You cannot build here.".to_string())]);
        // The owner, the trusted and an operator pass; so does everything outside.
        assert_eq!(t.rpc_before(ANN, RPC_DECONSTRUCT, 777, "[]"), Verdict::Continue);
        assert_eq!(t.rpc_before(BO, RPC_CREATE, 0, BUILD), Verdict::Continue);
        t.mock().set_operator(CY, true);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Continue);
        t.mock().set_operator(CY, false);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, &build_at(500.0)), Verdict::Continue);
        net_object(&mut t, 2, 778, [500.0, 0.0, 0.0]);
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 778, "[]"), Verdict::Continue);
        // The sphere has radius 20 around (0, 0, 0); the request is at height 2 and depth -3.25, so x = 19.5 is inside and 19.7 is out.
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, &build_at(19.5)), Verdict::Veto);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, &build_at(19.7)), Verdict::Continue);
    }

    #[test]
    fn other_rpcs_and_things_that_cannot_be_read_pass() {
        let mut t = claimed_world();
        // Another RPC name: not ours.
        assert_eq!(t.rpc_before(CY, "InventoriesHandler.Move", 777, BUILD), Verdict::Continue);
        // An object that is not there (already gone), or an id of 0.
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 123_456, "[]"), Verdict::Continue);
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 0, "[]"), Verdict::Continue);
        // Arguments that are not the build request: let through, and logged once.
        let deep = "[".repeat(5000);
        let long = format!("[1,[0,0,0],{}0]", "0,".repeat(2000));
        for bad in ["", "[]", "[1]", "[1,2]", "[1,[1,2],3]", "[1,[1,2,3,4]]", "[1,[\"a\",\"b\",\"c\"]]", "{\"a\":1}", "garbage", "[1,[1e999,0,0]]", "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[1", deep.as_str(), long.as_str()] {
            assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, bad), Verdict::Continue, "{:?}", &bad[..bad.len().min(40)]);
        }
        assert_eq!(t.mock().logs().iter().filter(|(l, _)| *l == Level::Warn).count(), 1);
        assert!(t.mock().told().is_empty());
        // A client that is not in the roster cannot be identified: let through.
        assert_eq!(t.rpc_before(99, RPC_CREATE, 0, BUILD), Verdict::Continue);
    }

    #[test]
    fn with_no_claims_the_veto_does_nothing_and_asks_nothing() {
        let mut t = world();
        net_object(&mut t, 1, 777, [0.0; 3]);
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 777, "[]"), Verdict::Continue);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Continue);
        assert!(t.mock().told().is_empty() && t.mock().logs().is_empty());
    }

    #[test]
    fn a_player_who_keeps_pressing_build_is_told_once_in_three_seconds_not_every_time() {
        let mock = MockHost::new().with_limits();
        for (c, n) in [(ANN, "Ann"), (CY, "Cy")] {
            mock.add_player(player(c, n, [0.0; 3], false));
        }
        let mut t = Harness::<Claims>::start(mock).tick_every_ms(0);
        t.command(ANN, "claim", &["20"]);
        t.mock().clear();
        for _ in 0..500 {
            assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Veto);
        }
        assert_eq!(t.mock().told().len(), 1);
        t.advance_ms(3000);
        t.rpc_before(CY, RPC_CREATE, 0, BUILD);
        assert_eq!(t.mock().told().len(), 2);
        // The list of those told is bounded.
        for c in 1000..1100u64 {
            t.mock().add_player(player(c, "Z", [0.0; 3], false));
            t.rpc_before(c, RPC_CREATE, 0, BUILD);
        }
        assert!(t.plugin().told.len() <= TELL_MEMORY);
        assert!(t.mock().denied().is_empty());
    }

    #[test]
    fn a_player_who_gave_up_a_claim_can_be_built_next_to_again() {
        let mut t = claimed_world();
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Veto);
        t.command(ANN, "unclaim", &[]);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Continue);
    }

    // ---- State.

    #[test]
    fn claims_and_trust_survive_a_reload() {
        let mut t = claimed_world();
        t.reload();
        assert_eq!(t.plugin().claims.len(), 1);
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Veto);
        assert_eq!(t.rpc_before(BO, RPC_CREATE, 0, BUILD), Verdict::Continue);
        t.command(ANN, "claims", &[]);
        assert_eq!(last_told(&t), "Your claims (1 of 3): #1 at (0, 0, 0) radius 20, trusted: Bo;");
        // Giving up the last claim is kept too.
        t.command(ANN, "unclaim", &[]);
        let saved = t.save().expect("always some state");
        assert_eq!((saved.version, saved.bytes.as_slice()), (STATE_VERSION, &[0u8, 0, 0, 0][..]));
        t.reload();
        assert!(t.plugin().claims.is_empty());
    }

    #[test]
    fn a_damaged_or_hostile_state_keeps_what_is_good_and_never_panics() {
        let mut t = claimed_world();
        let good = t.save().unwrap().bytes;
        // Cut anywhere: loads without a panic and logs when something was lost.
        for cut in 0..good.len() {
            let t = Harness::<Claims>::start_with_state(MockHost::new(), 1, &good[..cut]);
            assert!(t.plugin().claims.len() <= 1);
        }
        let t = Harness::<Claims>::start_with_state(MockHost::new(), 1, &good);
        assert_eq!(t.plugin().claims.len(), 1);
        assert!(t.mock().logs().is_empty());
        // A count far over what the bytes hold, and a claim count over the limit.
        let mut w = Writer::new();
        w.u32(u32::MAX);
        let t = Harness::<Claims>::start_with_state(MockHost::new(), 1, &w.0);
        assert!(t.plugin().claims.is_empty() && !t.mock().logs().is_empty());
        // Insane claims are dropped: no radius, NaN, infinity, a radius of a million metres.
        let mut w = Writer::new();
        w.u32(5);
        for (c, r) in [([0.0f32; 3], 0.0f32), ([f32::NAN, 0.0, 0.0], 10.0), ([f32::INFINITY, 0.0, 0.0], 10.0), ([0.0; 3], 1e6), ([1.0, 2.0, 3.0], 10.0)] {
            w.u64(1).str("Ann").f32(c[0]).f32(c[1]).f32(c[2]).f32(r).u32(0);
        }
        let t = Harness::<Claims>::start_with_state(MockHost::new(), 1, &w.0);
        assert_eq!(t.plugin().claims.len(), 1);
        // Too many trusted players are cut to the limit, and a name with control characters is cleaned.
        let mut w = Writer::new();
        w.u32(1).u64(1).str("An\nn").f32(0.0).f32(0.0).f32(0.0).f32(10.0).u32(20);
        for i in 0..20u64 {
            w.u64(i).str("T");
        }
        let t = Harness::<Claims>::start_with_state(MockHost::new(), 1, &w.0);
        assert_eq!((t.plugin().claims[0].trusted.len(), t.plugin().claims[0].owner_name.as_str()), (MAX_TRUSTED, "Ann"));
        // A version from the future: no claims, and a line in the log.
        let t = Harness::<Claims>::start_with_state(MockHost::new(), 2, &good);
        assert!(t.plugin().claims.is_empty() && t.mock().logs().iter().any(|(_, m)| m.contains("newer")));
    }

    // ---- The host refuses things.

    #[test]
    fn denied_capabilities_do_not_trap_and_the_game_goes_on() {
        // No world.read: the plugin cannot tell who is online or where anyone stands.
        let mock = MockHost::new().deny(&["world.read"]);
        let mut t = Harness::<Claims>::start(mock);
        t.plugin_mut().claims.push(Claim { owner: 5, owner_name: "Ann".into(), center: [0.0; 3], radius: 20.0, trusted: vec![] });
        t.command(CY, "claim", &["10"]);
        assert_eq!(last_told(&t), "I cannot see who is online right now, ask an admin.");
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Continue, "who cannot be told is let through");
        assert_eq!(t.rpc_before(CY, RPC_DECONSTRUCT, 7, "[]"), Verdict::Continue);
        // No chat.say: the veto still holds, and nobody is told.
        let mock = MockHost::new().deny(&["chat.say"]);
        mock.add_player(player(ANN, "Ann", [0.0; 3], false));
        mock.add_player(player(CY, "Cy", [0.0; 3], false));
        let mut t = Harness::<Claims>::start(mock);
        t.command(ANN, "claim", &["10"]);
        assert_eq!(t.plugin().claims.len(), 1, "the claim is made even if the answer cannot be said");
        assert_eq!(t.rpc_before(CY, RPC_CREATE, 0, BUILD), Verdict::Veto);
        assert!(t.mock().told().is_empty() && !t.mock().denied().is_empty());
    }

    #[test]
    fn a_list_of_claims_fits_in_a_few_lines_far_below_sixteen_actions() {
        let mock = MockHost::new().with_limits();
        mock.add_player(player(ANN, "Ann", [0.0; 3], false));
        let mut t = Harness::<Claims>::start(mock).tick_every_ms(0);
        for i in 0..20 {
            t.plugin_mut().claims.push(Claim {
                owner: id(ANN),
                owner_name: "Ann".into(),
                center: [-12345.678 * i as f32, 1.0, 2.0],
                radius: 200.0,
                trusted: (0..MAX_TRUSTED as u64).map(|k| Trusted { id: k, name: format!("Player number {k} with a long name") }).collect(),
            });
        }
        t.command(ANN, "claims", &[]);
        let told = t.mock().told();
        assert!(told.len() > 1 && told.len() <= 16, "{}", told.len());
        assert!(told.iter().all(|(_, m)| m.chars().count() <= 500));
        assert!(t.mock().denied().is_empty());
        // And the whole of it is there: all twenty numbers.
        let all: String = told.iter().map(|(_, m)| m.as_str()).collect();
        assert!(all.contains("#20 at"), "{all}");
    }

    #[test]
    fn a_flood_of_commands_and_rpcs_makes_no_more_than_sixteen_actions_a_call() {
        let mock = MockHost::new().with_limits();
        for (c, n) in [(ANN, "Ann"), (CY, "Cy")] {
            mock.add_player(player(c, n, [0.0; 3], false));
        }
        let mut t = Harness::<Claims>::start(mock).tick_every_ms(0);
        for i in 0..300 {
            t.command(ANN, ["claim", "unclaim", "trust", "untrust", "claims"][i % 5], &["Cy"]);
            t.rpc_before(CY, RPC_CREATE, 0, BUILD);
        }
        assert!(t.mock().denied().is_empty());
    }

    #[test]
    fn a_changed_name_is_shown_on_the_next_use() {
        let mut t = world();
        t.command(ANN, "claim", &["10"]);
        t.mock().add_player(player(ANN, "Anna\u{7}", [0.0; 3], false));
        t.command(ANN, "claims", &[]);
        t.mock().add_player(player(CY, "Cy", [0.0; 3], false));
        t.rpc_before(CY, RPC_CREATE, 0, BUILD);
        assert!(last_told(&t).contains("claimed by Anna."), "{}", last_told(&t));
    }

    #[test]
    fn the_build_position_is_read_by_the_short_way_or_by_the_json_reader_and_never_panics() {
        let want = Some([1.5, 2.0, -3.25]);
        assert_eq!(build_position(BUILD), want);
        // Spaces and line breaks (the quick way gives up; the JSON reader reads it).
        assert_eq!(build_position("[ -12345 , [ 1.5 , 2.0 , -3.25 ] , true ]"), want);
        assert_eq!(build_position("[1,[1.5,2e0,-3.25e0]]"), want);
        assert_eq!(build_position("[1,\n[1.5,\n2.0,-3.25]]"), want);
        for bad in ["", "[", "[1,[", "[1,[1,2]]", "[1,[1,2,3,4]]", "[1,[NaN,0,0]]", "[1,[inf,0,0]]", "[1,[\"1\",2,3]]", "[1,[1,2,3", "1,[1,2,3]]", "[[1,2,3]]", "null"] {
            assert_eq!(build_position(bad), None, "{bad:?}");
        }
        for cut in 0..BUILD.len() {
            let _ = build_position(&BUILD[..cut]);
        }
    }
}
