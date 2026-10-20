//! Conservative aerial clearance. Docking shafts may cross walls; cruise never crosses bounds.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

type V3 = [f32; 3];
const CELL: f32 = 64.0;
const CLEARANCE: f32 = 3.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bounds {
    pub min: V3,
    pub max: V3,
}
impl Bounds {
    pub fn transformed(&self, position: V3, rotation: [f32; 4]) -> Self {
        let rotation = if rotation == [0.0; 4] { [0.0, 0.0, 0.0, 1.0] } else { rotation };
        let center = [0, 1, 2].map(|i| (self.min[i] + self.max[i]) * 0.5);
        let half = [0, 1, 2].map(|i| (self.max[i] - self.min[i]) * 0.5);
        let center = super::rotate(rotation, center);
        let axes = [0, 1, 2].map(|j| {
            let mut v = [0.0; 3];
            v[j] = half[j];
            super::rotate(rotation, v)
        });
        let half = [0, 1, 2].map(|i| axes.iter().map(|a| a[i].abs()).sum::<f32>());
        Self {
            min: [0, 1, 2].map(|i| position[i] + center[i] - half[i]),
            max: [0, 1, 2].map(|i| position[i] + center[i] + half[i]),
        }
    }
    fn valid(&self) -> bool {
        (0..3).all(|i| self.min[i].is_finite() && self.max[i].is_finite() && self.min[i] <= self.max[i] && self.min[i].abs().max(self.max[i].abs()) < 1e6)
    }
    fn intersects(&self, a: V3, b: V3, axes: &[usize]) -> bool {
        let (mut near, mut far) = (0.0f32, 1.0f32);
        for &i in axes {
            let (low, high, delta) = (self.min[i] - CLEARANCE, self.max[i] + CLEARANCE, b[i] - a[i]);
            if delta.abs() < 1e-6 {
                if a[i] < low || a[i] > high {
                    return false;
                }
            } else {
                let (x, y) = ((low - a[i]) / delta, (high - a[i]) / delta);
                near = near.max(x.min(y));
                far = far.min(x.max(y));
                if near > far {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Data {
    pub format: u32,
    pub build_id: String,
    pub planets: HashMap<i32, Vec<Bounds>>,
    pub buildings: HashMap<String, Vec<Bounds>>,
}
impl Data {
    pub fn validate(&self, build: &str) -> Result<(), String> {
        if self.format != 1 || self.build_id != build.trim() {
            return Err("drone collision map format or game build differs; regenerate with pcds drone-map".into());
        }
        if !self.planets.values().chain(self.buildings.values()).flatten().all(Bounds::valid) {
            return Err("drone collision map contains invalid bounds".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Grid {
    bounds: Vec<Bounds>,
    cells: HashMap<(i32, i32), Vec<usize>>,
    large: Vec<usize>,
}
fn cell(x: f32) -> i32 {
    (x / CELL).floor() as i32
}
impl Grid {
    pub fn new(bounds: Vec<Bounds>) -> Self {
        let mut grid = Self { bounds, ..Default::default() };
        for (i, b) in grid.bounds.iter().enumerate() {
            let (x0, x1, z0, z1) = (
                cell(b.min[0] - CLEARANCE),
                cell(b.max[0] + CLEARANCE),
                cell(b.min[2] - CLEARANCE),
                cell(b.max[2] + CLEARANCE),
            );
            if i64::from(x1 - x0 + 1) * i64::from(z1 - z0 + 1) > 4096 {
                grid.large.push(i);
                continue;
            }
            for x in x0..=x1 {
                for z in z0..=z1 {
                    grid.cells.entry((x, z)).or_default().push(i);
                }
            }
        }
        grid
    }
    fn candidates(&self, a: V3, b: V3) -> HashSet<usize> {
        // Walk cells along the segment, rather than all cells in its bounding rectangle.
        let steps = (((b[0] - a[0]).abs().max((b[2] - a[2]).abs()) / (CELL * 0.5)).ceil() as usize).max(1);
        if steps > 4096 {
            return (0..self.bounds.len()).collect();
        }
        let mut found: HashSet<_> = self.large.iter().copied().collect();
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let (x, z) = (cell(a[0] + (b[0] - a[0]) * t), cell(a[2] + (b[2] - a[2]) * t));
            // Neighbour cells include corner crossings and the drone's clearance radius.
            for dx in -1..=1 {
                for dz in -1..=1 {
                    if let Some(v) = self.cells.get(&(x + dx, z + dz)) {
                        found.extend(v);
                    }
                }
            }
        }
        found
    }
    // shortcut: collider bounds overestimate some mountains; use mesh-level routing if detours dominate.
    pub fn route(&self, from: V3, to: V3) -> Vec<V3> {
        let candidates = self.candidates(from, to);
        if !candidates.iter().any(|i| self.bounds[*i].intersects(from, to, &[0, 1, 2])) {
            return vec![to];
        }
        let height = candidates
            .iter()
            .map(|i| &self.bounds[*i])
            .filter(|b| b.intersects(from, to, &[0, 2]))
            .fold(from[1].max(to[1]), |h, b| h.max(b.max[1] + CLEARANCE + 0.1));
        vec![[from[0], height, from[2]], [to[0], height, to[2]], to]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cruise_clears_all_bounds_and_docking_is_vertical() {
        let bounds = vec![
            Bounds {
                min: [10.0, 0.0, -4.0],
                max: [20.0, 40.0, 4.0],
            },
            Bounds {
                min: [40.0, 0.0, -2.0],
                max: [45.0, 60.0, 2.0],
            },
        ];
        let grid = Grid::new(bounds);
        let route = grid.route([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]);
        assert_eq!(route.len(), 3);
        assert!(route[0][1] > 63.0);
        assert_eq!(route[0][0], 0.0);
        assert_eq!(route[1][0], 60.0);
        assert!(grid.bounds.iter().all(|b| !b.intersects(route[0], route[1], &[0, 1, 2])));
        assert_eq!(grid.route([0.0, 100.0, 0.0], [60.0, 100.0, 0.0]).len(), 1);
        assert_eq!(grid.route([0.0, 5.0, 100.0], [60.0, 5.0, 100.0]).len(), 1);
    }
    #[test]
    fn indexed_routes_match_full_scan_across_negative_cells_and_long_diagonals() {
        let bounds: Vec<_> = (0..5000)
            .map(|i| {
                let x = (i % 100) as f32 * 50.0 - 2500.0;
                let z = (i / 100) as f32 * 50.0 - 1250.0;
                Bounds {
                    min: [x, 0.0, z],
                    max: [x + 5.0, (i % 70) as f32, z + 5.0],
                }
            })
            .collect();
        let grid = Grid::new(bounds.clone());
        let full = Grid {
            bounds,
            large: (0..5000).collect(),
            ..Default::default()
        };
        for i in 0..50 {
            let a = [-3000.0, 10.0, i as f32 * 63.0 - 1600.0];
            let b = [3000.0, 10.0, 1600.0 - i as f32 * 63.0];
            assert_eq!(grid.route(a, b), full.route(a, b));
        }
        assert!(grid.candidates([-100.0, 2.0, -100.0], [0.0, 2.0, 0.0]).len() < 100);
    }
    #[test]
    fn invalid_or_stale_maps_are_refused() {
        let mut d = Data {
            format: 1,
            build_id: "abc".into(),
            ..Default::default()
        };
        assert!(d.validate("abc\n").is_ok());
        assert!(d.validate("xyz").is_err());
        d.planets.insert(
            1,
            vec![Bounds {
                min: [f32::NAN; 3],
                max: [0.0; 3],
            }],
        );
        assert!(d.validate("abc").is_err());
    }
}
