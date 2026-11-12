//! The store conformance suite. A store passes by calling
//! [`store_conformance!`] from its tests, which makes one test per case, each
//! on a fresh store built by the expression given:
//!
//! ```ignore
//! store_conformance::store_conformance!(MyStore::new(store_conformance::DIMENSION));
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::assigning_clones,
    clippy::cloned_ref_to_slice_refs,
    reason = "this crate is a test suite: a panic is how a case fails"
)]

mod archive;
pub mod cases;
mod fixtures;

/// The embedding width the suite's store must be built with.
pub const DIMENSION: usize = 3;

/// Generates one `#[tokio::test]` per conformance case. The caller needs
/// `tokio` as a dev-dependency.
///
/// A store whose cases share one database passes `held` and an expression
/// giving `(store, guard)`. The guard is held until the case ends, so the
/// cases can take turns with the database.
#[macro_export]
macro_rules! store_conformance {
    (@list [$store:ident] $bind:pat = $make:expr) => {
        $crate::store_conformance!(@cases [$store] ($bind) $make;
            note_round_trip, a_missing_note_is_none, creating_a_duplicate_note_conflicts,
            update_checks_the_version, find_note_by_hash_finds_the_copy,
            deleting_a_note_removes_its_chunks, chunks_list_in_order,
            text_search_keeps_digits, text_search_ignores_case, text_search_with_no_match_is_empty,
            hits_carry_provenance, search_follows_replaced_chunks, public_only_hides_private,
            vector_search_orders_by_similarity, vector_search_skips_chunks_without_embeddings,
            vector_search_refuses_the_wrong_width, a_wrong_width_embedding_is_refused,
            search_pages_report_more, a_zero_vector_is_handled, an_empty_text_query_is_empty,
            hits_leave_out_the_embedding,
            unit_of_work_spans_kinds, unit_of_work_is_atomic,
            tasks_filter_by_status_and_due_time, working_memory_expires,
            job_keeps_who_requested_it, jobs_claim_by_priority_then_time, a_future_job_is_not_claimed,
            a_job_is_claimed_only_by_a_worker_for_its_kind,
            a_job_is_claimed_once, a_lapsed_lease_is_claimed_again, completing_needs_the_lease,
            a_failing_job_retries_then_dies, a_lease_lapsing_on_the_last_attempt_is_dead,
            a_lease_must_end_after_now, a_claim_tie_goes_to_the_lowest_id, failing_needs_the_lease,
            finished_jobs_are_pruned_by_age,
            a_lapsed_holder_finishes_only_until_the_job_is_claimed_again,
            lists_page_on_from_an_offset,
            a_definition_has_one_home, definitions_list_by_kind,
            an_undeclared_record_type_is_refused, an_invalid_record_is_refused,
            a_loose_record_schema_is_refused, a_badly_named_type_is_refused, a_failed_registration_adds_nothing,
            redeclaring_is_fine_but_changing_conflicts,
            records_filter_by_equality_and_range, timestamps_compare_as_times,
            a_filter_on_an_undeclared_field_is_refused, a_filter_of_the_wrong_type_is_refused,
            neighbors_follow_both_directions, a_link_needs_both_records,
            deleting_a_record_removes_its_links,
            spend_stays_within_the_cap, a_refused_reservation_changes_nothing,
            settling_replaces_the_reservation, spend_is_per_provider_and_per_day,
            reservations_at_once_cannot_overrun, huge_amounts_are_refused_not_wrapped,
            settling_never_goes_below_zero, jobs_list_newest_first_and_by_status,
            the_archive_lists_everything_in_order, a_restored_job_keeps_how_it_ended
        );
    };
    (@cases [$store:ident] ($bind:pat) $make:expr; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let $bind = $make;
                $crate::cases::$case(&$store).await;
            }
        )+
    };
    (held $make:expr) => {
        $crate::store_conformance!(@list [store] (store, _held) = $make);
    };
    ($make:expr) => {
        $crate::store_conformance!(@list [store] store = $make);
    };
}
