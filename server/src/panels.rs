//! Building panels (walls and floors of habitats): PanelsResources maps a panel's sub type to
//! the group whose recipe it costs, and Panel.GetContingousPanels opens compartments built side
//! by side into each other (and closes them again when one is taken apart).

use serde_json::Value;

use crate::game::Game;
use crate::geometry::{conj, mul, q, rotate, sub, v3};
use crate::handlers::change_panel;
use crate::net::{enc, NetBehaviour, NetObj, VarValue};

/// DataConfig.BuildPanelSubType.
pub const SUB_TYPES: [&str; 17] = ["WallNull", "WallPlain", "WallCorridor", "WallGlass", "WallDoor", "FloorLight", "FloorGlass", "FloorPlain", "FloorLab", "WallLab", "FloorNoLight", "WallInside", "None", "WallWaterLife", "FloorAnglePlain", "FloorAngleGlass", "FloorAngleLight"];
const WALL_CORRIDOR: i32 = 2;

/// GameConfig.undergroundConstructionLocation: below it panels take their underground look.
const UNDERGROUND: f32 = -500.0;

/// PanelsResources.GetPanelGroupConstructible.
pub fn panel_group(game: &Game, sub_type: i32) -> Option<usize> {
    let name = SUB_TYPES.get(sub_type as usize)?;
    let r = &game.data.singletons["PanelsResources"];
    let i = r["panelsSubtypes"].as_array()?.iter().position(|s| s.as_str() == Some(name))?;
    game.data.group_of_ref(r["panelsGroupItems"].get(i)?.as_str()?)
}

/// One Panel of a building, in the world.
struct WorldPanel<'a> {
    /// The building's network object (0 while it is being placed) and the behaviour's index.
    object: u64,
    index: u16,
    /// The dumped panel: panel type, transform in the building, box colliders.
    data: &'a Value,
    position: [f32; 3],
    rotation: [f32; 4],
    sub_type: i32,
}

/// Panel.GetSubPanelType of a Panel behaviour.
pub fn sub_type(b: &NetBehaviour) -> i32 {
    match b.vars.iter().find(|v| v.name == "_currentSubPanelType").map(|v| &v.value) {
        Some(VarValue::Plain(v)) if v.len() == 4 => i32::from_le_bytes(v[..4].try_into().unwrap()),
        _ => 0,
    }
}

/// The Panels of a building's network object, in behaviour order (GetComponentsInChildren).
fn panels_of<'a>(game: &'a Game, o: &NetObj) -> Vec<WorldPanel<'a>> {
    let Some(w) = o.wo.and_then(|w| game.wos.get(&w)) else { return Vec::new() };
    let Some(data) = game.data.panels["groups"][&game.data.groups[w.group].id].as_array() else { return Vec::new() };
    o.behaviours
        .iter()
        .filter(|b| b.class == "Panel")
        .zip(data)
        .map(|(b, d)| WorldPanel {
            object: o.id,
            index: b.index,
            data: d,
            position: add(o.position, rotate(o.rotation, v3(&d["position"]))),
            rotation: mul(o.rotation, q(&d["rotation"])),
            sub_type: sub_type(b),
        })
        .collect()
}

/// Every panel of the buildings standing on a planet: the colliders a ray there can meet.
fn placed_panels(game: &Game, planet: i32) -> Vec<WorldPanel<'_>> {
    game.net.objects.values().filter(|o| o.wo.and_then(|w| game.wos.get(&w)).is_some_and(|w| w.planet_hash() == planet)).flat_map(|o| panels_of(game, o)).collect()
}

/// BoundsHelper.GetBoundsOfChildren (`of_panel` false) or Panel.GetBoundsOfPanel of a panel:
/// the bounds of its look, as [min, max] in the world.
fn bounds(game: &Game, p: &WorldPanel, of_panel: bool) -> Option<[[f32; 3]; 2]> {
    let look = &game.data.panels["looks"][SUB_TYPES.get(p.sub_type as usize)?];
    let look = if p.position[1] < UNDERGROUND && look["underground"].is_object() { &look["underground"] } else { look };
    let b = look.get(if of_panel { "panelBounds" } else { "bounds" })?;
    let (min, max) = (v3(&b[0]), v3(&b[1]));
    // ponytail: the look's box turned with the panel, where the game bounds each renderer
    // turned; the same at right angles, a little larger at other angles.
    let mut out = [[f32::MAX; 3], [f32::MIN; 3]];
    for corner in 0..8 {
        let c = add(p.position, rotate(p.rotation, [0, 1, 2].map(|i| if corner >> i & 1 == 0 { min[i] } else { max[i] })));
        for i in 0..3 {
            out[0][i] = out[0][i].min(c[i]);
            out[1][i] = out[1][i].max(c[i]);
        }
    }
    Some(out)
}

fn center(b: [[f32; 3]; 2]) -> [f32; 3] {
    [0, 1, 2].map(|i| (b[0][i] + b[1][i]) / 2.0)
}

/// Where a ray meets one of a panel's box colliders, within `length`. As with Physics.Raycast,
/// a ray starting inside a box does not meet it.
fn hit(p: &WorldPanel, origin: [f32; 3], direction: [f32; 3], length: f32) -> Option<f32> {
    let (o, d) = (rotate(conj(p.rotation), sub(origin, p.position)), rotate(conj(p.rotation), direction));
    p.data["boxes"]
        .as_array()?
        .iter()
        .filter_map(|b| {
            let (c, s) = (v3(&b[0]), v3(&b[1]));
            let (lo, hi) = ([0, 1, 2].map(|i| c[i] - s[i] / 2.0), [0, 1, 2].map(|i| c[i] + s[i] / 2.0));
            if (0..3).all(|i| o[i] >= lo[i] && o[i] <= hi[i]) {
                return None;
            }
            let (mut near, mut far) = (0.0f32, length);
            for i in 0..3 {
                if d[i].abs() < 1e-9 {
                    if o[i] < lo[i] || o[i] > hi[i] {
                        return None;
                    }
                    continue;
                }
                let (a, b) = ((lo[i] - o[i]) / d[i], (hi[i] - o[i]) / d[i]);
                near = near.max(a.min(b));
                far = far.min(a.max(b));
                if near > far {
                    return None;
                }
            }
            Some(near)
        })
        .min_by(f32::total_cmp)
}

/// Panel.GetContingousPanels(2, forceAligned: true): the panel of the same type that the ray
/// from panel `i`'s look meets (outwards for walls; for floors from 5 above, along -up), with
/// panel bounds centred within 0.25 of this one's on two axes or more.
fn contiguous(game: &Game, panels: &[WorldPanel], i: usize) -> Option<usize> {
    let me = &panels[i];
    let ty = me.data["panelType"].as_str()?;
    let mut origin = center(bounds(game, me, false)?);
    if ty == "Floor" || ty == "FloorAngle" {
        origin[1] += 5.0;
    }
    let direction = rotate(me.rotation, if ty == "Wall" { [-1.0, 0.0, 0.0] } else { [0.0, -1.0, 0.0] });
    let mine = center(bounds(game, me, true)?);
    let aligned = |other: &WorldPanel| bounds(game, other, true).is_some_and(|b| (0..3).filter(|&a| (mine[a] - center(b)[a]).abs() < 0.25).count() >= 2);
    // ponytail: RaycastNonAlloc lists hits in no set order; the nearest aligned panel wins.
    panels
        .iter()
        .enumerate()
        .filter(|(j, p)| *j != i && p.data["panelType"].as_str() == Some(ty))
        .filter_map(|(j, p)| Some((hit(p, origin, direction, 2.0)?, j)))
        .filter(|(_, j)| aligned(&panels[*j]))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, j)| j)
}

/// WorldObjectsHandler.ChangeContingousPanels for a building being placed (`o`, not spawned
/// yet): Panel.ChangePanelAndContigousPanel turns each of its walls that meets a neighbour's
/// wall, and that wall, into a corridor. The neighbour changes at once; the new building's own
/// walls change as it spawns (Panel.ChangePanel before OnNetworkSpawn), so the host's spawn
/// message already shows them open.
pub fn change_contiguous_panels(game: &mut Game, o: &mut NetObj) {
    let Some(planet) = o.wo.and_then(|w| game.wos.get(&w)).map(|w| w.planet_hash()) else { return };
    // Only walls join their neighbours: without one there is nothing to look for.
    if !panels_of(game, o).iter().any(|p| p.data["panelType"] == "Wall") {
        return;
    }
    let (mut own, mut neighbours) = (Vec::new(), Vec::new());
    {
        let mut panels = placed_panels(game, planet);
        let first = panels.len();
        panels.extend(panels_of(game, o));
        for i in first..panels.len() {
            if panels[i].data["panelType"] != "Wall" {
                continue;
            }
            if let Some(n) = contiguous(game, &panels, i) {
                own.push(panels[i].index);
                if n >= first {
                    own.push(panels[n].index);
                } else {
                    neighbours.push((panels[n].object, panels[n].index));
                }
            }
        }
    }
    for (object, index) in neighbours {
        change_panel(game, object, index, WALL_CORRIDOR, true, false);
    }
    // ChangePanelServerRpc on the new building, refreshing its panels' ids.
    let mut changed = false;
    for index in own {
        let Some(v) = o.behaviours.iter_mut().find(|b| b.index == index).and_then(|b| b.vars.iter_mut().find(|v| v.name == "_currentSubPanelType")) else { continue };
        if !matches!(&v.value, VarValue::Plain(b) if *b == enc::enumeration(WALL_CORRIDOR)) {
            v.value = VarValue::Plain(enc::enumeration(WALL_CORRIDOR));
            game.panel_constructed(WALL_CORRIDOR);
            changed = true;
        }
    }
    if let Some(w) = o.wo.and_then(|w| game.wos.get_mut(&w)).filter(|_| changed) {
        w.panels = Some(o.behaviours.iter().filter(|b| b.class == "Panel").map(sub_type).collect());
    }
}

/// Panel.ResetContigousPanel on every panel of a building being taken apart: the panel each one
/// meets goes back to plain.
pub fn reset_contiguous_panels(game: &mut Game, object: u64) {
    let Some(planet) = game.net.objects.get(&object).and_then(|o| o.wo).and_then(|w| game.wos.get(&w)).map(|w| w.planet_hash()) else { return };
    // A building without panels has none to reset.
    if game.net.objects.get(&object).is_none_or(|o| panels_of(game, o).is_empty()) {
        return;
    }
    let resets: Vec<(u64, u16, i32)> = {
        let panels = placed_panels(game, planet);
        (0..panels.len())
            .filter(|&i| panels[i].object == object)
            .filter_map(|i| contiguous(game, &panels, i))
            .filter_map(|n| {
                let plain = match panels[n].data["panelType"].as_str()? {
                    "Wall" => "WallPlain",
                    "Floor" => "FloorPlain",
                    "FloorAngle" => "FloorAnglePlain",
                    _ => return None,
                };
                Some((panels[n].object, panels[n].index, SUB_TYPES.iter().position(|s| *s == plain)? as i32))
            })
            .collect()
    };
    for (object, index, sub_type) in resets {
        change_panel(game, object, index, sub_type, true, false);
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[cfg(test)]
mod tests {
    use crate::game::tests::save;
    use crate::game::{test_game, Game};

    /// A compartment ("pod": 6 m inside, 1 m walls) placed at `position`, turned `yaw` degrees.
    fn place(g: &mut Game, position: [f32; 3], yaw: f32) -> (i32, u64) {
        let id = g.create_world_object(g.data.group("pod").unwrap(), 0, true);
        let planet = g.planet_hash();
        let w = g.wos.get_mut(&id).unwrap();
        (w.position, w.rotation, w.planet) = (position, crate::geometry::from_euler_y(yaw), planet);
        (id, g.instantiate(id, false, 0).unwrap())
    }

    #[test]
    fn compartments_side_by_side_open_into_each_other_and_close_again() {
        let Some(mut g) = test_game(save()) else { return };
        if g.data.panels.is_null() {
            return eprintln!("skipped: tables without panels");
        }
        // Walls in order: +z, -z, +x, -x (wall 3 of one faces wall 4 of the other 8 m along x).
        let (a, _) = place(&mut g, [600.0, 10.0, 500.0], 0.0);
        let (b, b_net) = place(&mut g, [592.0, 10.0, 500.0], 0.0);
        assert_eq!(g.wos[&a].panels, Some(vec![1, 1, 1, 2, 5, 7]), "the neighbour's -x wall is a corridor");
        assert_eq!(g.wos[&b].panels, Some(vec![1, 1, 2, 1, 5, 7]), "and the new one's +x wall");
        // Turned a quarter, the same pair lies along z.
        let (c, _) = place(&mut g, [700.0, 10.0, 500.0], 90.0);
        let (d, _) = place(&mut g, [700.0, 10.0, 508.0], 90.0);
        assert_eq!((g.wos[&c].panels.clone(), g.wos[&d].panels.clone()), (Some(vec![1, 1, 1, 2, 5, 7]), Some(vec![1, 1, 2, 1, 5, 7])));
        // The game aligns panel centres on world axes: a diagonal pair stays closed.
        let (e, _) = place(&mut g, [800.0, 10.0, 500.0], 45.0);
        let (f, _) = place(&mut g, [800.0 - 8.0 * 0.70710677, 10.0, 500.0 + 8.0 * 0.70710677], 45.0);
        assert_eq!((g.wos[&e].panels.clone(), g.wos[&f].panels.clone()), (None, None));
        crate::deconstruct::deconstruct(&mut g, 1, b_net, 0.0);
        assert_eq!(g.wos[&a].panels, Some(vec![1, 1, 1, 1, 5, 7]), "taking one apart closes the other");
    }
}
