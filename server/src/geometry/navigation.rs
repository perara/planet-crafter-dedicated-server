//! Conservative aerial clearance. Docking shafts may cross walls; cruise never crosses bounds.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    visits: Vec<u32>,
    query: u32,
}
fn cell(x: f32) -> i32 {
    (x / CELL).floor() as i32
}
impl Grid {
    pub fn new(bounds: Vec<Bounds>) -> Self {
        let mut grid = Self { visits: vec![0; bounds.len()], bounds, ..Default::default() };
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
    fn candidates(&mut self, a: V3, b: V3) -> Vec<usize> {
        let (mut x, mut z, end_x, end_z) = (cell(a[0]), cell(a[2]), cell(b[0]), cell(b[2]));
        if i64::from(end_x).abs_diff(i64::from(x)) + i64::from(end_z).abs_diff(i64::from(z)) > 4096 {
            return (0..self.bounds.len()).collect();
        }
        self.query = self.query.wrapping_add(1);
        if self.query == 0 {
            self.visits.fill(0);
            self.query = 1;
        }
        let mut found = Vec::new();
        let mut collect = |items: &[usize]| {
            for &i in items {
                if self.visits[i] != self.query {
                    self.visits[i] = self.query;
                    found.push(i);
                }
            }
        };
        collect(&self.large);
        let (dx, dz) = ((end_x - x).signum(), (end_z - z).signum());
        let crossing = |axis: usize, c: i32, step: i32| {
            if step == 0 { f64::INFINITY } else {
                (f64::from(c + i32::from(step > 0)) * f64::from(CELL) - f64::from(a[axis])) / (f64::from(b[axis]) - f64::from(a[axis]))
            }
        };
        loop {
            if let Some(v) = self.cells.get(&(x, z)) { collect(v); }
            if x == end_x && z == end_z { break; }
            let (tx, tz) = (if x == end_x { f64::INFINITY } else { crossing(0, x, dx) }, if z == end_z { f64::INFINITY } else { crossing(2, z, dz) });
            // Bounds are already padded in the index; only crossed cells are needed.
            // Include both side cells at a corner, even when floating point rounds the tie.
            if (tx - tz).abs() <= 1e-10 {
                if let Some(v) = self.cells.get(&(x + dx, z)) { collect(v); }
                if let Some(v) = self.cells.get(&(x, z + dz)) { collect(v); }
                x += dx;
                z += dz;
            } else if tx < tz {
                x += dx;
            } else {
                z += dz;
            }
        }
        found
    }
    // shortcut: collider bounds overestimate some mountains; use mesh-level routing if detours dominate.
    pub fn route_with(&mut self, other: Option<&mut Self>, from: V3, to: V3) -> Vec<V3> {
        let (blocked, height) = self.clearance(from, to);
        let (other_blocked, other_height) = other.map(|g| g.clearance(from, to)).unwrap_or((false, from[1].max(to[1])));
        if !blocked && !other_blocked {
            return vec![to];
        }
        let height = height.max(other_height);
        vec![[from[0], height, from[2]], [to[0], height, to[2]], to]
    }

    fn clearance(&mut self, from: V3, to: V3) -> (bool, f32) {
        let mut blocked = false;
        let mut height = from[1].max(to[1]);
        for i in self.candidates(from, to) {
            let b = &self.bounds[i];
            if b.intersects(from, to, &[0, 2]) {
                height = height.max(b.max[1] + CLEARANCE + 0.1);
                blocked |= b.intersects(from, to, &[0, 1, 2]);
            }
        }
        (blocked, height)
    }

    #[cfg(test)]
    pub fn route(&mut self, from: V3, to: V3) -> Vec<V3> {
        self.route_with(None, from, to)
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
        let mut grid = Grid::new(bounds);
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
    fn terrain_detours_also_clear_overhead_buildings() {
        let mut terrain = Grid::new(vec![Bounds {
            min: [10.0, 0.0, -5.0],
            max: [20.0, 40.0, 5.0],
        }]);
        let mut overhead = Grid::new(vec![Bounds {
            min: [30.0, 35.0, -5.0],
            max: [40.0, 80.0, 5.0],
        }]);
        let (from, to) = ([0.0, 5.0, 0.0], [60.0, 5.0, 0.0]);
        assert_eq!(overhead.route(from, to), vec![to], "the original low route passes below the building");
        let route = terrain.route_with(Some(&mut overhead), from, to);
        assert!(route[0][1] > 83.0, "the raised cruise also must clear overhead geometry");
        assert!(terrain.bounds.iter().chain(&overhead.bounds).all(|b| !b.intersects(route[0], route[1], &[0, 1, 2])));
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
        let mut grid = Grid::new(bounds.clone());
        let mut full = Grid::new(bounds);
        full.cells.clear();
        full.large = (0..5000).collect();
        for i in 0..50 {
            let a = [-3000.0, 10.0, i as f32 * 63.0 - 1600.0];
            let b = [3000.0, 10.0, 1600.0 - i as f32 * 63.0];
            assert_eq!(grid.route(a, b), full.route(a, b));
        }
        assert!(grid.candidates([-100.0, 2.0, -100.0], [0.0, 2.0, 0.0]).len() < 100);
    }

    #[test]
    fn crossed_cells_match_full_scan_at_edges_corners_and_in_both_directions() {
        let bounds: Vec<_> = (-12..=12).flat_map(|x| (-12..=12).map(move |z| Bounds {
            min: [x as f32 * CELL - CLEARANCE, -10.0, z as f32 * CELL - CLEARANCE],
            max: [x as f32 * CELL + CLEARANCE, (x * x + z * z) as f32, z as f32 * CELL + CLEARANCE],
        })).chain(std::iter::once(Bounds { min: [-10000.0, -20.0, -10000.0], max: [10000.0, -10.0, 10000.0] })).collect();
        let mut grid = Grid::new(bounds.clone());
        let mut full = Grid::new(bounds);
        full.cells.clear();
        full.large = (0..full.bounds.len()).collect();
        let mut rng = crate::game::Rng::fixed(81);
        for i in 0..1000 {
            let a = [rng.range(-1000, 1000) as f32, rng.range(-20, 400) as f32, rng.range(-1000, 1000) as f32];
            let mut b = [rng.range(-1000, 1000) as f32, rng.range(-20, 400) as f32, rng.range(-1000, 1000) as f32];
            match i % 5 {
                0 => b = a,
                1 => b[0] = a[0],
                2 => b[2] = a[2],
                3 => b = [a[2], b[1], a[0]],
                _ => {}
            }
            assert_eq!(grid.route(a, b), full.route(a, b), "{a:?} -> {b:?}");
            assert_eq!(grid.route(b, a), full.route(b, a), "{b:?} -> {a:?}");
        }
        for (a, b) in [
            ([-1024.0, 5.0, -1024.0], [1024.0, 5.0, 1024.0]),
            ([64.0, 5.0, -1024.0], [64.0, 5.0, 1024.0]),
            ([-1024.0, 5.0, 64.0], [1024.0, 5.0, 64.0]),
            ([-200000.0, 5.0, 0.0], [200000.0, 5.0, 0.0]),
        ] {
            assert_eq!(grid.route(a, b), full.route(a, b));
            assert_eq!(grid.route(b, a), full.route(b, a));
        }
        grid.query = u32::MAX;
        grid.visits.fill(1);
        let (a, b) = ([-500.0, 5.0, 0.0], [500.0, 5.0, 0.0]);
        assert_eq!(grid.route(a, b), full.route(a, b), "visit stamps reset when the counter wraps");
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
