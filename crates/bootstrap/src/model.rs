//! A model provider given by a test, as the gateway's [`ProviderAdapter`],
//! so the model gateway needs no change to accept one.

use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::{Error, ExecutionContext, ModelProvider, ModelRequest, ModelResponse};
use model_gateway::{ModelUsage, ProviderAdapter, ProviderCall};

/// One HTTP attempt per call, with the response's own usage.
pub(crate) struct Given(pub(crate) Arc<dyn ModelProvider>);

#[async_trait]
impl ModelProvider for Given {
    fn provider_id(&self) -> &Identifier {
        self.0.provider_id()
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        self.0.complete(request, ctx).await
    }
}

#[async_trait]
impl ProviderAdapter for Given {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        let response = self.0.complete(request, ctx).await?;
        Ok(ProviderCall {
            usage_by_model: vec![ModelUsage {
                model: response.model_id.clone(),
                usage: response.usage.clone(),
            }],
            response,
            attempts: 1,
            request_id: None,
        })
    }
}
