// Read-only scaffold excerpt from src/api/router/snapshot_content_tests.rs.

// Lines 544-548:
544: struct FixtureOptions {
545:     initial_facts: Vec<InitialFact>,
546:     initial_chunk_faults: Vec<(&'static str, bounded_chunks::ChunkFault)>,
547:     lease_seconds: Option<u64>,
548: }

// Lines 640-660:
640: impl Fixture {
641:     async fn new() -> Self {
642:         Self::new_with_pg_config(false).await
643:     }
644: 
645:     async fn new_with_pg_config(rebuildable: bool) -> Self {
646:         Self::new_with_pg_config_and_directories(rebuildable, 0).await
647:     }
648: 
649:     async fn new_with_pg_config_and_directories(rebuildable: bool, directory_count: usize) -> Self {
650:         Self::new_with_pg_config_directories_and_objects(rebuildable, directory_count, &[]).await
651:     }
652: 
653:     async fn new_with_pg_config_directories_and_objects(
654:         rebuildable: bool,
655:         directory_count: usize,
656:         objects: &[(String, Vec<u8>)],
657:     ) -> Self {
658:         Self::new_in_metadata_family(rebuildable, directory_count, objects, false).await
659:     }
660: 

// Lines 672-709:
672:     async fn new_in_metadata_family(
673:         rebuildable: bool,
674:         directory_count: usize,
675:         objects: &[(String, Vec<u8>)],
676:         generic_history: bool,
677:     ) -> Self {
678:         Self::new_in_publication_mode(rebuildable, directory_count, objects, generic_history, true)
679:             .await
680:     }
681: 
682:     async fn new_without_publication() -> Self {
683:         Self::new_in_publication_mode(true, 0, &[], false, false).await
684:     }
685: 
686:     async fn new_in_publication_mode(
687:         rebuildable: bool,
688:         directory_count: usize,
689:         objects: &[(String, Vec<u8>)],
690:         generic_history: bool,
691:         publication_enabled: bool,
692:     ) -> Self {
693:         Self::new_in_publication_mode_with_options(
694:             rebuildable,
695:             directory_count,
696:             objects,
697:             generic_history,
698:             publication_enabled,
699:             FixtureOptions::default(),
700:         )
701:         .await
702:     }
703: 
704:     async fn new_rooted_with_options(
705:         objects: &[(String, Vec<u8>)],
706:         options: FixtureOptions,
707:     ) -> Self {
708:         Self::new_in_publication_mode_with_options(false, 0, objects, false, true, options).await
709:     }

// Lines 711-718:
711:     async fn new_in_publication_mode_with_options(
712:         rebuildable: bool,
713:         directory_count: usize,
714:         objects: &[(String, Vec<u8>)],
715:         generic_history: bool,
716:         publication_enabled: bool,
717:         options: FixtureOptions,
718:     ) -> Self {

// Lines 965-983:
965:         let mut resolve_request = json!({"target":{"kind":"latest"},"scope":"/project"});
966:         if let Some(lease_seconds) = options.lease_seconds {
967:             resolve_request["lease_seconds"] = json!(lease_seconds);
968:         }
969:         let response = app
970:             .clone()
971:             .oneshot(
972:                 Request::builder()
973:                     .method("POST")
974:                     .uri("/api/v2/snapshots/resolve")
975:                     .header("authorization", format!("Bearer {TOKEN}"))
976:                     .header("content-type", "application/json")
977:                     .body(Body::from(resolve_request.to_string()))
978:                     .unwrap(),
979:             )
980:             .await
981:             .unwrap();
982:         let resolved = success_json(response).await;
983:         assert!(counts.whole.load(Ordering::SeqCst) > 0);
