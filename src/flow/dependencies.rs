use log::{debug, trace};
use rustc_middle::mir::*;
use rustc_mir_dataflow::{Results, ResultsRefCursor, ResultsVisitor};

use super::dataflow::{FlowAnalysis, FlowDomain};
use crate::core::{
  config::Range,
  indexed::IndexedDomain,
  indexed_impls::{LocationSet, PlaceSet},
  utils,
};

#[derive(Clone, Copy, Debug)]
pub enum Direction {
  Forward,
  Backward,
}

struct DepVisitor<'tcx> {
  direction: Direction,
  target_deps: Vec<LocationSet>,
  outputs: Vec<(LocationSet, PlaceSet<'tcx>)>,
}

impl DepVisitor<'tcx> {
  fn visit(&mut self, state: &FlowDomain<'tcx>, location: Location) {
    for (target_locs, (out_locs, out_places)) in
      self.target_deps.iter().zip(self.outputs.iter_mut())
    {
      for (place, loc_deps) in state.rows() {
        if loc_deps.len() == 0 {
          continue;
        }

        let matches = match self.direction {
          Direction::Forward => loc_deps.is_superset(target_locs),
          Direction::Backward => target_locs.is_superset(&loc_deps),
        };

        if matches {
          trace!(
            "{:?}: place {:?} (deps {:?}) / target_locs {:?}",
            location,
            state.row_domain.value(place),
            loc_deps,
            target_locs
          );
          out_places.insert(place);

          if loc_deps.contains(location) {
            out_locs.insert(location);
          }
        }
      }
    }
  }
}

impl ResultsVisitor<'mir, 'tcx> for DepVisitor<'tcx> {
  type FlowState = FlowDomain<'tcx>;

  fn visit_statement_after_primary_effect(
    &mut self,
    state: &Self::FlowState,
    _statement: &'mir Statement<'tcx>,
    location: Location,
  ) {
    self.visit(state, location);
  }

  fn visit_terminator_after_primary_effect(
    &mut self,
    state: &Self::FlowState,
    _terminator: &'mir rustc_middle::mir::Terminator<'tcx>,
    location: Location,
  ) {
    self.visit(state, location);
  }
}

pub fn compute_dependencies(
  results: &Results<'tcx, FlowAnalysis<'mir, 'tcx>>,
  targets: Vec<(Place<'tcx>, Location)>,
  direction: Direction,
) -> Vec<(LocationSet, PlaceSet<'tcx>)> {
  let tcx = results.analysis.tcx;
  let body = results.analysis.body;
  let aliases = &results.analysis.aliases;

  let new_location_set = || LocationSet::new(results.analysis.location_domain().clone());
  let new_place_set = || PlaceSet::new(results.analysis.place_domain().clone());

  let expanded_targets = targets
    .iter()
    .map(|(place, location)| {
      let mut places = new_place_set();
      places.insert(*place);

      for (_, ptrs) in utils::interior_pointers(*place, tcx, body, results.analysis.def_id) {
        for (place, _) in ptrs {
          debug!(
            "{:?} // {:?}",
            tcx.mk_place_deref(place),
            aliases.aliases.row_set(tcx.mk_place_deref(place))
          );
          places.union(&aliases.aliases.row_set(tcx.mk_place_deref(place)).unwrap());
        }
      }

      (places, *location)
    })
    .collect::<Vec<_>>();
  debug!(
    "Expanded targets from {:?} to {:?}",
    targets, expanded_targets
  );

  let target_deps = {
    let mut cursor = ResultsRefCursor::new(body, results);
    let get_deps = |(targets, location): &(PlaceSet<'tcx>, Location)| {
      cursor.seek_after_primary_effect(*location);
      let state = cursor.get();

      let mut locations = new_location_set();
      for target in targets.indices() {
        if let Some(dep_locations) = state.row_set(target) {
          locations.union(&dep_locations);
        }
      }

      locations
    };
    expanded_targets.iter().map(get_deps).collect::<Vec<_>>()
  };
  debug!("Target deps: {:?}", target_deps);

  let mut outputs = target_deps
    .iter()
    .map(|_| (new_location_set(), new_place_set()))
    .collect::<Vec<_>>();
  for ((target_places, _), (_, places)) in expanded_targets.iter().zip(outputs.iter_mut()) {
    places.union(target_places);
  }

  let mut visitor = DepVisitor {
    direction,
    target_deps,
    outputs,
  };
  results.visit_reachable_with(body, &mut visitor);
  debug!("visitor.outputs: {:?}", visitor.outputs);

  visitor.outputs
}

pub fn compute_dependency_ranges(
  results: &Results<'tcx, FlowAnalysis<'mir, 'tcx>>,
  targets: Vec<(Place<'tcx>, Location)>,
  direction: Direction,
  spanner: &utils::HirSpanner,
) -> Vec<Vec<Range>> {
  let tcx = results.analysis.tcx;
  let body = results.analysis.body;

  let source_map = tcx.sess.source_map();
  let deps = compute_dependencies(results, targets, direction);

  deps
    .into_iter()
    .map(|(locations, places)| {
      let location_spans = locations
        .iter()
        .map(|location| utils::location_to_spans(*location, body, spanner, source_map))
        .flatten();

      let place_spans = places
        .iter()
        .filter(|place| **place != Place::return_place())
        .map(|place| {
          body.local_decls()[place.local]
            .source_info
            .span
            .source_callsite()
        });

      location_spans
        .chain(place_spans)
        .filter_map(|span| Range::from_span(span, source_map).ok())
        .collect::<Vec<_>>()
    })
    .collect::<Vec<_>>()
}
