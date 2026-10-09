//! The target grids cdors describes itself for planning (`remap::target::from_name`) must equal
//! cdo's templates (`cdo -f nc4 const,0,<name>`) as the reader sees them: dimensions, variables
//! and their attributes, the grid, and every coordinate and bounds value bit for bit.
//!
//! Needs cdo (`$CDO` or on `PATH`); skipped without it. Templates are written under
//! `$CARGO_TARGET_TMPDIR/native-grids` (never removed).

use cdors_core::io::ChunkSource;
use cdors_core::model::VarKind;
use cdors_core::remap::target::from_name;
use cdors_core::remap::{WeightCache, generate::find_cdo};
use std::path::PathBuf;
use std::sync::Arc;

const NAMES: &[&str] = &[
    "r18x9",
    "r36x18",
    "r360x180",
    "R7/5",
    "r1x1",
    "r10_3",
    "global",
    "global_1",
    "global_30",
    "global_0.25",
    "global_7",
    "global_2.5",
    "hpz0",
    "hpz2",
    "hpz2_ring",
    "hpz3_nest",
    "hp2",
    "hp4",
    "hp4_ring",
    "hp8_nested",
    "lon=10_lat=20",
    "lon=10/lat=20",
    "lon=-30.5xlat=45.25",
    "LON=1e1_LAT=-5",
];

#[test]
fn native_grids_equal_cdo_templates() {
    let Some(cdo) = find_cdo() else {
        eprintln!("skipped: no cdo");
        return;
    };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("native-grids");
    let cache = WeightCache::new(dir, Some(cdo));
    for &name in NAMES {
        let tpath = cache.grid_template(name).expect("cdo template");
        let t = cdors_core::io::open(&tpath.to_string_lossy()).expect("open template");
        let n: Arc<dyn ChunkSource> =
            Arc::new(from_name(name).unwrap_or_else(|| panic!("{name}: not described")));
        let (td, nd) = (t.dataset(), n.dataset());
        assert_eq!(td.dims, nd.dims, "{name}: dimensions");
        assert_eq!(td.vars.len(), nd.vars.len(), "{name}: variables");
        for (a, b) in td.vars.iter().zip(&nd.vars) {
            assert_eq!(a.name, b.name, "{name}: variable names");
            assert_eq!(a.kind, b.kind, "{name}: kind of {}", a.name);
            assert_eq!(a.dtype, b.dtype, "{name}: type of {}", a.name);
            assert_eq!(a.dims, b.dims, "{name}: dimensions of {}", a.name);
            assert_eq!(a.attrs, b.attrs, "{name}: attributes of {}", a.name);
            assert_eq!(a.grid, b.grid, "{name}: grid of {}", a.name);
            if matches!(
                a.kind,
                VarKind::Coordinate | VarKind::Auxiliary | VarKind::Bounds
            ) {
                let (va, vb) = (t.read_var(&a.name).unwrap(), n.read_var(&b.name).unwrap());
                let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
                assert_eq!(bits(&va), bits(&vb), "{name}: values of {}", a.name);
            }
        }
        assert_eq!(td.grids.len(), 1, "{name}: one grid");
        assert_eq!(
            format!("{:?}", td.grids[0]),
            format!("{:?}", nd.grids[0]),
            "{name}: grid"
        );
        let (ga, gb) = (&td.grids[0], &nd.grids[0]);
        for (a, b) in [(&ga.xvals, &gb.xvals), (&ga.yvals, &gb.yvals)] {
            let bits = |v: &Option<Vec<f64>>| {
                v.as_ref()
                    .map(|v| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>())
            };
            assert_eq!(bits(a), bits(b), "{name}: grid coordinates");
        }
        assert!(
            ga.x.as_ref().and_then(|x| x.bounds_var.as_ref()).is_none(),
            "{name}: the template has bounds"
        );
        eprintln!("{name}: equal ({} cells)", ga.size);
    }
}
