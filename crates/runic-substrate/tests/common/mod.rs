//! Shared contract test harness for `runic-substrate`.
//!
//! The traits (`SessionStore`, `ArtifactStore`) promise the same behavior
//! across every backend, so the *contract* lives here once and each backend
//! runs the whole suite. Backend-specific risks (Postgres tx/concurrency,
//! Local path safety, Memory aliasing) get their own files on top.
//!
//! Each contract case is a `pub async fn case(store: &dyn Trait)` that uses
//! freshly-generated unique ids — backends can share one physical DB and stay
//! logically isolated. A `*_contract_suite!` macro stamps out one
//! `#[tokio::test]` per case given a factory that yields a fresh store (or
//! `None` to skip the whole suite, e.g. Postgres with no DATABASE_URL set).

#![allow(dead_code)]

pub mod artifact_contract;
pub mod ids;
pub mod session_contract;
pub mod stress_contract;

/// Emit one `#[tokio::test]` per named contract case in `$module`.
///
/// `$factory` is an expression evaluated fresh inside every test — a closure
/// returning a future of `Option<Store>`. `None` skips that case (returns
/// early), which is how a Postgres suite no-ops when no test DB is configured.
#[macro_export]
macro_rules! contract_suite {
    ($module:path, $factory:expr, $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                // `use … as` aliases the module: a `:path` fragment can't be
                // followed by `::` directly in expansion, but it can by `as`.
                use $module as cases;
                let factory = $factory;
                let Some(store) = factory().await else { return };
                cases::$case(&store).await;
            }
        )+
    };
}

/// Like [`contract_suite!`] but marks each generated test `#[ignore]` — for the
/// slow volume/stress cases, run on demand with `cargo test -- --ignored`.
#[macro_export]
macro_rules! contract_suite_ignored {
    ($module:path, $factory:expr, $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "stress: slow, run with --ignored"]
            async fn $case() {
                use $module as cases;
                let factory = $factory;
                let Some(store) = factory().await else { return };
                cases::$case(&store).await;
            }
        )+
    };
}

/// Volume/stress cases for `SessionStore` (ignored by default).
#[macro_export]
macro_rules! session_store_stress_suite {
    ($factory:expr) => {
        $crate::contract_suite_ignored!(
            $crate::common::stress_contract,
            $factory,
            ten_thousand_events_one_thread,
            one_thousand_threads_one_tenant,
            hundred_tenants_same_thread_name,
            append_batch_sizes_1_10_100_1000,
            paginate_two_thousand_events_small_page,
            reconstruct_large_log,
        );
    };
}

/// Volume/stress cases for `ArtifactStore` (ignored by default).
#[macro_export]
macro_rules! artifact_store_stress_suite {
    ($factory:expr) => {
        $crate::contract_suite_ignored!(
            $crate::common::stress_contract,
            $factory,
            one_thousand_artifacts_one_session,
        );
    };
}

/// The full `SessionStore` contract — one canonical list, run by every backend.
#[macro_export]
macro_rules! session_store_contract_suite {
    ($factory:expr) => {
        $crate::contract_suite!(
            $crate::common::session_contract,
            $factory,
            empty_read_returns_empty,
            append_one_then_read_one,
            append_many_preserves_insertion_order,
            append_batch_preserves_order,
            multiple_batches_preserve_global_order,
            seqs_are_monotonic_and_gapless,
            read_after_zero_returns_all,
            read_after_last_seq_returns_empty,
            pagination_covers_every_event_once,
            read_run_after_filters_by_run,
            append_isolated_across_sessions,
            append_isolated_across_tenants,
            same_session_id_different_tenants_isolated,
            weird_tenant_and_session_ids_isolated,
            event_payload_roundtrip_exact_all_variants,
            timestamps_roundtrip_microsecond,
            run_ids_roundtrip_exactly,
            large_text_payload_roundtrips,
            unicode_payload_roundtrips,
            empty_text_payload_roundtrips,
            list_sessions_shows_appended_session,
            list_sessions_is_tenant_scoped,
            list_sessions_orders_recent_first,
            list_sessions_page_covers_every_session_once,
            set_label_reflected_in_meta_and_list,
            set_label_materializes_empty_session,
            set_label_none_clears,
            set_label_does_not_disturb_event_count,
            delete_session_removes_from_read_and_list,
            delete_session_is_tenant_scoped,
            recreate_after_delete_has_clean_log,
            session_meta_absent_for_unknown,
            reconstruct_completed_run,
            reconstruct_in_flight_run,
            reconstruct_terminal_run_preserves_stop_reason,
            reconstruct_multiple_runs_in_order,
            reconstruct_tool_call_and_result_messages,
            snapshot_replaces_messages_on_replay,
        );
    };
}

/// Optional full-text search contract — only backends that implement `search`
/// (Memory, Postgres) opt in. Single-word queries only, so memory's substring
/// match and Postgres FTS agree; richer query syntax is Postgres-specific.
#[macro_export]
macro_rules! session_store_search_suite {
    ($factory:expr) => {
        $crate::contract_suite!(
            $crate::common::session_contract,
            $factory,
            search_finds_other_session_same_tenant,
            search_excludes_current_session,
            search_is_tenant_scoped,
            search_respects_limit,
            search_empty_when_no_match,
        );
    };
}

/// The full `ArtifactStore` contract — run by every backend.
#[macro_export]
macro_rules! artifact_store_contract_suite {
    ($factory:expr) => {
        $crate::contract_suite!(
            $crate::common::artifact_contract,
            $factory,
            put_get_roundtrip_exact_bytes,
            head_returns_metadata,
            list_returns_session_artifacts,
            list_is_tenant_session_scoped,
            get_after_delete_is_notfound,
            head_after_delete_is_notfound,
            delete_is_idempotent,
            empty_bytes_roundtrip,
            binary_bytes_with_zeros_roundtrip,
            artifact_ids_are_unique,
            many_artifacts_in_session_list,
            mime_type_roundtrips,
            source_roundtrips_all_variants,
            created_at_is_sane,
            list_order_is_deterministic,
            weird_tenant_session_no_collision,
            malicious_id_get_is_notfound_not_traversal,
            weird_names_do_not_escape_or_cross,
        );
    };
}

/// The stronger delete contract (`list` excludes a deleted artifact). All
/// backends run it — Memory/Postgres drop the row, Local skips index entries
/// whose blob is gone.
#[macro_export]
macro_rules! artifact_store_delete_from_list_suite {
    ($factory:expr) => {
        $crate::contract_suite!(
            $crate::common::artifact_contract,
            $factory,
            list_after_delete_excludes_artifact,
        );
    };
}
