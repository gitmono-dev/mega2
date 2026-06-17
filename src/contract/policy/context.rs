use cedar_policy::{
    Authorizer, Context, Decision, PolicySet, Request, Schema, ValidationMode, Validator,
};
use itertools::Itertools;

use crate::{
    common::errors::{ContextError, SaturnContextError},
    contract::policy::{entitystore::EntityStore, util::SaturnEUid},
};

pub struct CedarContext {
    pub entities: EntityStore,
    authorizer: Authorizer,
    policies: PolicySet,
    schema: Schema,
}

#[allow(clippy::result_large_err)]
impl CedarContext {
    pub fn from(entities: EntityStore, policy_content: &str) -> Result<Self, ContextError> {
        let (schema, _) = Schema::from_cedarschema_str(include_str!("mega.cedarschema"))?;
        let policies = policy_content.parse()?;
        let validator = Validator::new(schema.clone());
        let output = validator.validate(&policies, ValidationMode::default());

        if output.validation_passed() {
            tracing::debug!("All policy validation passed!");
            let authorizer = Authorizer::new();
            let c = Self {
                entities,
                authorizer,
                policies,
                schema,
            };

            Ok(c)
        } else {
            let error_string = output
                .validation_errors()
                .map(|err| format!("{err}"))
                .join("\n");
            Err(ContextError::Validation(error_string))
        }
    }

    pub fn new(entities: EntityStore) -> Result<Self, ContextError> {
        let schema_content = include_str!("mega.cedarschema");
        let policy_content = include_str!("mega_policies.cedar");
        let (schema, _) = Schema::from_cedarschema_str(schema_content).unwrap();
        let policies = policy_content.parse()?;
        let validator = Validator::new(schema.clone());
        let output = validator.validate(&policies, ValidationMode::default());

        if output.validation_passed() {
            tracing::debug!("All policy validation passed!");
            let authorizer = Authorizer::new();
            let c = Self {
                entities,
                authorizer,
                policies,
                schema,
            };

            Ok(c)
        } else {
            let error_string = output
                .validation_errors()
                .map(|err| format!("{err}"))
                .join("\n");
            Err(ContextError::Validation(error_string))
        }
    }

    pub fn is_authorized(
        &self,
        principal: impl AsRef<SaturnEUid>,
        action: impl AsRef<SaturnEUid>,
        resource: impl AsRef<SaturnEUid>,
        context: Context,
    ) -> Result<(), SaturnContextError> {
        let es = self.entities.as_entities(&self.schema);
        let q = Request::new(
            principal.as_ref().clone().into(),
            action.as_ref().clone().into(),
            resource.as_ref().clone().into(),
            context,
            Some(&self.schema),
        )
        .map_err(|e| SaturnContextError::Request(e.to_string()))?;
        tracing::debug!(
            "is_authorized request: principal: {}, action: {}, resource: {}",
            principal.as_ref(),
            action.as_ref(),
            resource.as_ref()
        );
        let response = self.authorizer.is_authorized(&q, &self.policies, &es);
        tracing::debug!("Auth response: {:?}", response);
        match response.decision() {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(SaturnContextError::AuthDenied(
                response.diagnostics().clone(),
            )),
        }
    }
}
