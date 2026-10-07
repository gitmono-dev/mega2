//! Explicit preparation termination. Payload deletion remains forbidden.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataTerminalAction {
    Abort,
    RetireCoverage,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataTerminalError {
    #[error(transparent)]
    Rejected(#[from] SnapshotError),
    #[error("metadata {action:?} commit outcome is unknown for operation {operation_id}")]
    CommitUncertain {
        operation_id: String,
        manifest_digest: [u8; 32],
        action: MetadataTerminalAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataTerminalReceipt {
    intent: GenerationPrepareIntent,
    action: MetadataTerminalAction,
    terminal_at: sea_orm::prelude::DateTimeWithTimeZone,
}

impl MetadataTerminalReceipt {
    pub fn intent(&self) -> &GenerationPrepareIntent {
        &self.intent
    }

    pub fn action(&self) -> MetadataTerminalAction {
        self.action
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataTerminalObservation {
    Active,
    Terminated(Box<MetadataTerminalReceipt>),
}

impl PostgresMetadataGenerationRepository {
    pub async fn abort(
        &self,
        intent: &GenerationPrepareIntent,
    ) -> Result<MetadataTerminalReceipt, MetadataTerminalError> {
        self.terminate(intent, MetadataTerminalAction::Abort, None)
            .await
    }

    pub async fn retire_prepare_coverage(
        &self,
        receipt: &GenerationMetadataReceipt,
    ) -> Result<MetadataTerminalReceipt, MetadataTerminalError> {
        self.terminate(
            &receipt.intent,
            MetadataTerminalAction::RetireCoverage,
            Some(receipt),
        )
        .await
    }

    async fn terminate(
        &self,
        intent: &GenerationPrepareIntent,
        action: MetadataTerminalAction,
        receipt: Option<&GenerationMetadataReceipt>,
    ) -> Result<MetadataTerminalReceipt, MetadataTerminalError> {
        self.require_scope(intent)?;
        let txn = self.inner.transaction().await?;
        let result = self.terminate_in_txn(&txn, intent, action, receipt).await;
        match result {
            Ok(receipt) => {
                txn.commit().await.map_err(|_| uncertain(intent, action))?;
                Ok(receipt)
            }
            Err(error) => {
                txn.rollback().await.map_err(internal)?;
                Err(error.into())
            }
        }
    }

    async fn terminate_in_txn(
        &self,
        txn: &DatabaseTransaction,
        intent: &GenerationPrepareIntent,
        action: MetadataTerminalAction,
        receipt: Option<&GenerationMetadataReceipt>,
    ) -> Result<MetadataTerminalReceipt, SnapshotError> {
        self.inner.barrier(txn).await?;
        lock_prepare(txn, intent.prepare_id()).await?;
        let record = self.require_terminal_record(txn, intent).await?;
        if let Some(terminal) = terminal_receipt(&record, intent, action)? {
            verify_no_prepare_coverage(txn, intent).await?;
            return Ok(terminal);
        }
        let expected = match action {
            MetadataTerminalAction::Abort => "PREPARING",
            MetadataTerminalAction::RetireCoverage => "COMMITTED",
        };
        if record.state != expected {
            return Err(SnapshotError::new(
                SnapshotErrorCode::Conflict,
                "metadata prepare is in another terminal state",
            ));
        }
        let fixed = self.require_fixed_plan(txn, intent).await?;
        if action == MetadataTerminalAction::RetireCoverage {
            let receipt = receipt
                .ok_or_else(|| integrity("coverage retirement requires a definitive receipt"))?;
            if receipt.legacy != fixed.stored.receipt()? || receipt.intent != fixed.intent {
                return Err(integrity(
                    "coverage retirement receipt differs from its fixed preparation",
                ));
            }
            check_generation_payloads(txn, &fixed).await?;
            verify_graph(txn, &fixed.stored).await?;
        }
        let roots = mst2_retention_root::Entity::find()
            .filter(
                mst2_retention_root::Column::RootKey.eq(format!("prepare:{}", intent.prepare_id())),
            )
            .limit((MetadataDagLimits::default().nodes + 1) as u64)
            .all(txn)
            .await
            .map_err(internal)?;
        let expected_nodes: BTreeSet<_> = fixed.bindings.0.keys().map(node_id).collect();
        if roots
            .iter()
            .any(|root| root.root_kind != "prepare" || !expected_nodes.contains(&root.node_id))
        {
            return Err(integrity(
                "terminal prepare coverage differs from its fixed lifetime mappings",
            ));
        }
        if action == MetadataTerminalAction::RetireCoverage
            && roots
                .iter()
                .map(|root| root.node_id.clone())
                .collect::<BTreeSet<_>>()
                != expected_nodes
        {
            return Err(unavailable(
                "coverage retirement cannot recover missing or transferred prepare roots",
            ));
        }
        txn.execute_raw(statement(
            "DELETE FROM mst2_retention_root r USING mst2_metadata_prepare_page p
             WHERE p.prepare_id=$1 AND r.root_key='prepare:'||p.prepare_id AND r.root_kind='prepare'
               AND r.node_id='page:sha256:'||encode(p.page_id,'hex')",
            [intent.prepare_id().into()],
        ))
        .await
        .map_err(internal)?;
        let sql=match action {
            MetadataTerminalAction::Abort=>
                "UPDATE mst2_metadata_prepare SET state='ABORTED',aborted_at=clock_timestamp()
                 WHERE prepare_id=$1 AND state='PREPARING' AND storage_seal=$2 RETURNING *",
            MetadataTerminalAction::RetireCoverage=>
                "UPDATE mst2_metadata_prepare SET coverage_retired_at=clock_timestamp()
                 WHERE prepare_id=$1 AND state='COMMITTED' AND coverage_retired_at IS NULL AND storage_seal=$2 RETURNING *",
        };
        let row = txn
            .query_one_raw(statement(
                sql,
                [
                    intent.prepare_id().into(),
                    intent.storage_seal.to_vec().into(),
                ],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| integrity("metadata terminal CAS changed during its barrier"))?;
        let column = match action {
            MetadataTerminalAction::Abort => "aborted_at",
            MetadataTerminalAction::RetireCoverage => "coverage_retired_at",
        };
        let terminal_at = row.try_get("", column).map_err(internal)?;
        Ok(MetadataTerminalReceipt {
            intent: intent.clone(),
            action,
            terminal_at,
        })
    }

    /// A fresh same-primary barrier resolves a lost terminal commit response.
    /// An old receipt is replayed without consulting or changing a newer lifetime.
    pub async fn inspect_terminal(
        &self,
        fresh_primary: &DatabaseConnection,
        intent: &GenerationPrepareIntent,
        action: MetadataTerminalAction,
    ) -> Result<MetadataTerminalObservation, MetadataTerminalError> {
        self.require_scope(intent)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| uncertain(intent, action))?;
        if self.inner.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(uncertain(intent, action));
        }
        let result = async {
            let record = self.require_terminal_record(&txn, intent).await?;
            match terminal_receipt(&record, intent, action)? {
                Some(receipt) => {
                    verify_no_prepare_coverage(&txn, intent).await?;
                    Ok(MetadataTerminalObservation::Terminated(Box::new(receipt)))
                }
                None => Ok(MetadataTerminalObservation::Active),
            }
        }
        .await;
        txn.rollback()
            .await
            .map_err(|_| uncertain(intent, action))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                uncertain(intent, action)
            } else {
                MetadataTerminalError::Rejected(error)
            }
        })
    }

    async fn require_terminal_record<C: ConnectionTrait>(
        &self,
        connection: &C,
        intent: &GenerationPrepareIntent,
    ) -> Result<mst2_metadata_prepare::Model, SnapshotError> {
        self.require_scope(intent)?;
        let record = mst2_metadata_prepare::Entity::find()
            .filter(mst2_metadata_prepare::Column::OperationId.eq(intent.operation_id()))
            .one(connection)
            .await
            .map_err(internal)?
            .ok_or_else(|| unavailable("sealed metadata preparation is missing"))?;
        let canonical = record
            .canonical_bindings
            .as_deref()
            .ok_or_else(|| unavailable("legacy preparation has no generation seal"))?;
        let plan = MetadataInstallPlan::decode(&record.canonical_plan, &intent.manifest_digest())?;
        let bindings = GenerationBindings::decode(canonical, &plan)?;
        let actual_digest: [u8; 32] = Sha256::digest(canonical).into();
        let domain = GraphDomain::from_stored(record.graph_domain.as_deref())?;
        if record.prepare_id != intent.prepare_id()
            || record.manifest_digest.as_slice() != intent.manifest_digest()
            || record.metadata_root.as_slice() != intent.metadata_root
            || record.bindings_digest.as_deref() != Some(intent.bindings_digest.as_slice())
            || actual_digest != intent.bindings_digest
            || record.primary_scope.as_deref() != Some(intent.primary_scope.as_ref())
            || record.storage_seal.as_deref() != Some(intent.storage_seal.as_slice())
            || domain != intent.graph_domain
            || bindings.0.get(&intent.metadata_root).map(|pair| pair.0)
                != Some(intent.root_generation)
            || seal(
                intent.prepare_id(),
                &intent.manifest_digest(),
                &intent.metadata_root,
                &intent.bindings_digest,
                &intent.primary_scope,
                domain.stored(),
            )? != intent.storage_seal
        {
            return Err(integrity(
                "terminal metadata action differs from its immutable generation seal",
            ));
        }
        let pages = mst2_metadata_prepare_page::Entity::find()
            .filter(mst2_metadata_prepare_page::Column::PrepareId.eq(intent.prepare_id()))
            .limit((MetadataDagLimits::default().nodes + 1) as u64)
            .all(connection)
            .await
            .map_err(internal)?;
        let mut actual = BTreeMap::new();
        for page in pages {
            let id: [u8; 32] = page.page_id.as_slice().try_into().map_err(internal)?;
            actual.insert(
                id,
                (
                    page.generation
                        .ok_or_else(|| integrity("terminal preparation has an unbound mapping"))?,
                    page.expected_size as u64,
                ),
            );
        }
        if actual != bindings.0 {
            return Err(integrity(
                "terminal metadata mappings differ from the fixed seal",
            ));
        }
        Ok(record)
    }
}

fn terminal_receipt(
    record: &mst2_metadata_prepare::Model,
    intent: &GenerationPrepareIntent,
    action: MetadataTerminalAction,
) -> Result<Option<MetadataTerminalReceipt>, SnapshotError> {
    let terminal_at = match action {
        MetadataTerminalAction::Abort if record.state == "ABORTED" => Some(
            record
                .aborted_at
                .ok_or_else(|| integrity("aborted metadata preparation has no terminal receipt"))?,
        ),
        MetadataTerminalAction::RetireCoverage if record.state == "COMMITTED" => {
            record.coverage_retired_at
        }
        _ => None,
    };
    Ok(terminal_at.map(|terminal_at| MetadataTerminalReceipt {
        intent: intent.clone(),
        action,
        terminal_at,
    }))
}

async fn verify_no_prepare_coverage<C: ConnectionTrait>(
    connection: &C,
    intent: &GenerationPrepareIntent,
) -> Result<(), SnapshotError> {
    if connection
        .query_one_raw(statement(
            "SELECT node_id FROM mst2_retention_root WHERE root_key='prepare:'||$1 LIMIT 1",
            [intent.prepare_id().into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity(
            "terminal metadata preparation still has prepare coverage",
        ));
    }
    Ok(())
}

fn uncertain(
    intent: &GenerationPrepareIntent,
    action: MetadataTerminalAction,
) -> MetadataTerminalError {
    MetadataTerminalError::CommitUncertain {
        operation_id: intent.operation_id().into(),
        manifest_digest: intent.manifest_digest(),
        action,
    }
}

#[cfg(test)]
#[path = "native_metadata_history_tests.rs"]
mod tests;
