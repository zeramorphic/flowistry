use super::{
  aliases::Aliases,
  borrow_ranges::BorrowRanges,
  place_index::PlaceSet,
  place_index::{PlaceIndex, PlaceIndices},
};
use log::debug;
use rustc_middle::{
  mir::{
    self,
    borrows::BorrowSet,
    visit::{PlaceContext, Visitor},
    *,
  },
  ty::{TyCtxt, TyKind},
};
use rustc_mir::{
  borrow_check::{borrow_conflicts_with_place, AccessDepth, PlaceConflictBias},
  dataflow::{
    fmt::{DebugWithAdapter, DebugWithContext},
    Analysis, AnalysisDomain, Backward, JoinSemiLattice, Results, ResultsRefCursor,
  },
};
use std::{cell::RefCell, collections::HashSet, fmt};

pub type SliceSet = HashSet<Location>;

// Previous strategy of representing path relevance as a bool didn't seem to work out
// with out dataflow framework handles start/exit states and join? Adding a third unknown
// state as bottom rather than defaulting to false seemed to work
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Relevant {
  Yes,
  No,
  Unknown,
}

impl JoinSemiLattice for Relevant {
  fn join(&mut self, other: &Self) -> bool {
    let state = match (*self, *other) {
      (Relevant::Yes, _) | (_, Relevant::Yes) => Relevant::Yes,
      (Relevant::No, _) | (_, Relevant::No) => Relevant::No,
      _ => Relevant::Unknown,
    };
    if state != *self {
      *self = state;
      true
    } else {
      false
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelevanceDomain {
  pub places: PlaceSet,
  pub statement_relevant: bool,
  pub path_relevant: Relevant,
}

impl JoinSemiLattice for RelevanceDomain {
  fn join(&mut self, other: &Self) -> bool {
    let places_changed = self.places.join(&other.places);
    let path_relevant_changed = self.path_relevant.join(&other.path_relevant);
    places_changed || path_relevant_changed
  }
}

impl DebugWithContext<RelevanceAnalysis<'_, '_, '_>> for RelevanceDomain {
  fn fmt_with(
    &self,
    ctxt: &RelevanceAnalysis<'_, '_, '_>,
    f: &mut fmt::Formatter<'_>,
  ) -> fmt::Result {
    self.places.fmt_with(ctxt.place_indices, f)?;
    write!(
      f,
      " {:?}, {:?}",
      self.statement_relevant, self.path_relevant
    )
  }
}

struct CollectPlaceIndices<'a, 'tcx> {
  places: PlaceSet,
  place_indices: &'a PlaceIndices<'tcx>,
}

impl<'a, 'tcx> Visitor<'tcx> for CollectPlaceIndices<'a, 'tcx> {
  fn visit_place(&mut self, place: &Place<'tcx>, _context: PlaceContext, _location: Location) {
    self.places.insert(self.place_indices.index(place));
  }
}

struct TransferFunction<'a, 'b, 'mir, 'tcx> {
  analysis: &'a RelevanceAnalysis<'b, 'mir, 'tcx>,
  state: &'a mut RelevanceDomain,
}

impl<'a, 'b, 'mir, 'tcx> TransferFunction<'a, 'b, 'mir, 'tcx> {
  fn add_relevant(&mut self, place: Place<'tcx>) {
    self
      .state
      .places
      .insert(self.analysis.place_indices.index(&place));
    self.state.statement_relevant = true;
    self.state.path_relevant = Relevant::Yes;
  }

  fn add_relevant_many(&mut self, places: &PlaceSet) {
    self.state.places.union(places);
    self.state.statement_relevant = true;
    self.state.path_relevant = Relevant::Yes;
  }

  fn any_relevant(&mut self, possibly_mutated: &PlaceSet) -> bool {
    possibly_mutated.iter().any(|mutated_place| {
      self.state.places.iter().any(|relevant_place| {
        self
          .analysis
          .place_index_is_part(mutated_place, relevant_place)
          || self
            .analysis
            .place_index_is_part(relevant_place, mutated_place)
      })
    })
  }
}

impl<'a, 'b, 'mir, 'tcx> Visitor<'tcx> for TransferFunction<'a, 'b, 'mir, 'tcx> {
  fn visit_statement(&mut self, statement: &Statement<'tcx>, location: Location) {
    self.state.statement_relevant = false;
    match &statement.kind {
      StatementKind::Assign(_) => {
        self.super_statement(statement, location);
      }
      _ => {}
    }
  }

  fn visit_assign(&mut self, place: &Place<'tcx>, rvalue: &Rvalue<'tcx>, location: Location) {
    self.super_assign(place, rvalue, location);

    macro_rules! fmt_places {
      ($places:expr) => {
        DebugWithAdapter {
          this: &$places,
          ctxt: self.analysis.place_indices,
        }
      };
    }

    debug!("checking {:?} = {:?}", place, rvalue);
    let (possibly_mutated, pointers_to_mutated) = self.analysis.places_and_pointers(*place);
    debug!(
      "  relevant {:?}, possibly_mutated {:?}, pointers_to_mutated {:?}",
      fmt_places!(self.state.places),
      fmt_places!(possibly_mutated),
      fmt_places!(pointers_to_mutated)
    );

    let any_relevant_mutated = self.any_relevant(&possibly_mutated);

    if any_relevant_mutated {
      // strong update
      if possibly_mutated.count() == 1 {
        debug!("  deleting {:?}", fmt_places!(possibly_mutated));
        let definitely_mutated = possibly_mutated.iter().next().unwrap();
        let to_delete = self
          .state
          .places
          .iter()
          .filter(|relevant_place| {
            self
              .analysis
              .place_index_is_part(*relevant_place, definitely_mutated)
          })
          .collect::<Vec<_>>();
        for i in to_delete {
          self.state.places.remove(i);
        }
        debug!("  after deletion: {:?}", fmt_places!(self.state.places));
      }

      let mut collector = CollectPlaceIndices {
        places: self.analysis.place_indices.empty_set(),
        place_indices: self.analysis.place_indices,
      };
      collector.visit_rvalue(rvalue, location);

      debug!(
        "  adding relevant places {:?} and pointers to possibly mutated {:?}",
        fmt_places!(collector.places),
        fmt_places!(pointers_to_mutated)
      );
      self.add_relevant_many(&collector.places);
      self.add_relevant_many(&pointers_to_mutated);
    }
  }

  fn visit_place(&mut self, place: &Place<'tcx>, _context: PlaceContext, location: Location) {
    if self.analysis.slice_set.contains(&location) {
      self.add_relevant(*place);
    }
  }

  fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, _location: Location) {
    self.state.statement_relevant = false;

    debug!(
      "checking terminator {:?} in context {:?}",
      terminator.kind, self.state.places
    );

    match &terminator.kind {
      TerminatorKind::Call {
        args, destination, ..
      } => {
        let input_places = args
          .iter()
          .filter_map(|arg| match arg {
            Operand::Move(place) | Operand::Copy(place) => Some(*place),
            Operand::Constant(_) => None,
          })
          .collect::<Vec<_>>();

        let any_mut_ptrs_to_relevant = input_places.iter().any(|arg| {
          let (places, _) = self.analysis.places_and_pointers(*arg);
          self.any_relevant(&places)
        });

        let dest_relevant = if let Some((dst, _)) = destination {
          let (possibly_mutated, _) = self.analysis.places_and_pointers(*dst);
          // TODO: strong update to delete dest
          self.any_relevant(&possibly_mutated)
        } else {
          false
        };

        if dest_relevant || any_mut_ptrs_to_relevant {
          for place in input_places {
            self.add_relevant(place);
          }
        }
      }

      TerminatorKind::SwitchInt { discr, .. } => {
        if self.state.path_relevant == Relevant::Yes {
          match discr {
            Operand::Move(place) | Operand::Copy(place) => {
              let (places, _) = self.analysis.places_and_pointers(*place);
              self.add_relevant_many(&places);
            }
            Operand::Constant(_) => {}
          }
        }
      }

      _ => {}
    }

    self.state.path_relevant = if self.state.statement_relevant {
      Relevant::Yes
    } else {
      Relevant::No
    };
  }
}

pub struct RelevanceAnalysis<'a, 'mir, 'tcx> {
  slice_set: SliceSet,
  tcx: TyCtxt<'tcx>,
  body: &'mir Body<'tcx>,
  borrow_set: &'a BorrowSet<'tcx>,
  borrow_ranges: RefCell<ResultsRefCursor<'a, 'mir, 'tcx, BorrowRanges<'mir, 'tcx>>>,
  place_indices: &'a PlaceIndices<'tcx>,
  aliases: RefCell<ResultsRefCursor<'a, 'mir, 'tcx, Aliases<'a, 'mir, 'tcx>>>,
}

impl<'a, 'mir, 'tcx> RelevanceAnalysis<'a, 'mir, 'tcx> {
  pub fn new(
    slice_set: SliceSet,
    tcx: TyCtxt<'tcx>,
    body: &'mir Body<'tcx>,
    borrow_set: &'a BorrowSet<'tcx>,
    borrow_ranges: &'a Results<'tcx, BorrowRanges<'mir, 'tcx>>,
    place_indices: &'a PlaceIndices<'tcx>,
    aliases: &'a Results<'tcx, Aliases<'a, 'mir, 'tcx>>,
  ) -> Self {
    let borrow_ranges = RefCell::new(ResultsRefCursor::new(body, &borrow_ranges));
    let aliases = RefCell::new(ResultsRefCursor::new(body, aliases));
    RelevanceAnalysis {
      slice_set,
      tcx,
      body,
      borrow_set,
      borrow_ranges,
      place_indices,
      aliases,
    }
  }

  fn place_index_is_part(&self, part_place: PlaceIndex, whole_place: PlaceIndex) -> bool {
    self.place_is_part(
      self.place_indices.lookup(part_place),
      self.place_indices.lookup(whole_place),
    )
  }

  fn place_is_part(&self, part_place: Place<'tcx>, whole_place: Place<'tcx>) -> bool {
    // borrow_conflicts_with_place considers it a bug if borrow_place is behind immutable deref, so special case this
    // see places_conflict.rs:234-236
    {
      let access_place = part_place;
      let borrow_place = whole_place;
      if borrow_place.projection.len() > access_place.projection.len() {
        for (i, _elem) in borrow_place.projection[access_place.projection.len()..]
          .iter()
          .enumerate()
        {
          let proj_base = &borrow_place.projection[..access_place.projection.len() + i];
          let base_ty = Place::ty_from(borrow_place.local, proj_base, self.body, self.tcx).ty;
          if let TyKind::Ref(_, _, Mutability::Not) = base_ty.kind() {
            return false;
          }
        }
      }
    }

    borrow_conflicts_with_place(
      self.tcx,
      self.body,
      whole_place,
      BorrowKind::Mut {
        allow_two_phase_borrow: true,
      },
      part_place.as_ref(),
      AccessDepth::Deep,
      PlaceConflictBias::Overlap,
    )
  }

  fn places_and_pointers(&self, place: Place<'tcx>) -> (PlaceSet, PlaceSet) {
    let borrow_ranges = self.borrow_ranges.borrow();
    let borrow_ranges = borrow_ranges.get();

    let aliases = self.aliases.borrow();
    let aliases = aliases.get();

    let mut places = self.place_indices.empty_set();
    let mut pointers = self.place_indices.empty_set();
    places.insert(self.place_indices.index(&place));

    for i in borrow_ranges.iter() {
      let borrow = &self.borrow_set[i];

      // Ignore immutable borrows for now
      if borrow.kind.to_mutbl_lossy() != Mutability::Mut {
        continue;
      }

      let mut borrow_aliases = aliases.iter_enumerated().filter_map(|(local, borrows)| {
        if borrows.contains(i) {
          Some(local)
        } else {
          None
        }
      });

      let part_of_alias = borrow_aliases.any(|alias| {
        self.place_is_part(
          place,
          Place {
            local: alias,
            projection: self.tcx.intern_place_elems(&[]),
          },
        )
      });

      if self.place_is_part(place, borrow.assigned_place) || part_of_alias {
        places.insert(self.place_indices.index(&borrow.borrowed_place));
        pointers.insert(self.place_indices.index(&place));
        pointers.insert(self.place_indices.index(&borrow.assigned_place));

        let (sub_places, sub_pointers) = self.places_and_pointers(borrow.borrowed_place);
        places.union(&sub_places);
        pointers.union(&sub_pointers);
      }
    }

    (places, pointers)
  }
}

impl<'a, 'mir, 'tcx> AnalysisDomain<'tcx> for RelevanceAnalysis<'a, 'mir, 'tcx> {
  type Domain = RelevanceDomain;
  type Direction = Backward;
  const NAME: &'static str = "RelevanceAnalysis";

  fn bottom_value(&self, _body: &mir::Body<'tcx>) -> Self::Domain {
    RelevanceDomain {
      places: self.place_indices.empty_set(),
      statement_relevant: false,
      path_relevant: Relevant::Unknown,
    }
  }

  fn initialize_start_block(&self, _: &mir::Body<'tcx>, _: &mut Self::Domain) {}
}

impl<'a, 'mir, 'tcx> Analysis<'tcx> for RelevanceAnalysis<'a, 'mir, 'tcx> {
  fn apply_statement_effect(
    &self,
    state: &mut Self::Domain,
    statement: &mir::Statement<'tcx>,
    location: Location,
  ) {
    self
      .borrow_ranges
      .borrow_mut()
      .seek_before_primary_effect(location);
    self
      .aliases
      .borrow_mut()
      .seek_before_primary_effect(location);

    TransferFunction {
      state,
      analysis: self,
    }
    .visit_statement(statement, location);
  }

  fn apply_terminator_effect(
    &self,
    state: &mut Self::Domain,
    terminator: &mir::Terminator<'tcx>,
    location: Location,
  ) {
    self
      .borrow_ranges
      .borrow_mut()
      .seek_before_primary_effect(location);
    self
      .aliases
      .borrow_mut()
      .seek_before_primary_effect(location);

    TransferFunction {
      state,
      analysis: self,
    }
    .visit_terminator(terminator, location);
  }

  fn apply_call_return_effect(
    &self,
    _state: &mut Self::Domain,
    _block: BasicBlock,
    _func: &mir::Operand<'tcx>,
    _args: &[mir::Operand<'tcx>],
    _return_place: mir::Place<'tcx>,
  ) {
  }
}
