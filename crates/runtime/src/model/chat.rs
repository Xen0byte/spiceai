/*
Copyright 2024-2025 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/
#![allow(clippy::implicit_hasher)]
use async_openai::{
    error::OpenAIError,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionResponseStream, ChatCompletionStreamOptions, CreateChatCompletionRequest,
        CreateChatCompletionResponse,
    },
};
use async_trait::async_trait;
use futures::Stream;
use futures::{stream::StreamExt, TryStreamExt};
use llms::openai::DEFAULT_LLM_MODEL;
use llms::{
    anthropic::{Anthropic, AnthropicConfig},
    chat::{nsql::SqlGeneration, Chat, Error as LlmError, Result as ChatResult},
    xai::Xai,
};
use secrecy::{ExposeSecret, SecretString};
use spicepod::component::model::{Model, ModelFileType, ModelSource};
use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc};
use std::{pin::Pin, time::Instant};
use tracing_futures::Instrument;

use super::{
    metrics::{handle_metrics, request_labels},
    tool_use::ToolUsingChat,
};
use crate::{
    tools::{options::SpiceToolsOptions, utils::get_tools},
    Runtime,
};

pub type LLMModelStore = HashMap<String, Box<dyn Chat>>;

/// Extract a secret from a hashmap of secrets, if it exists.
macro_rules! extract_secret {
    ($params:expr, $key:expr) => {
        $params.get($key).map(|s| s.expose_secret().as_str())
    };
}

/// Attempt to derive a runnable Chat model from a given component from the Spicepod definition.
pub async fn try_to_chat_model(
    component: &Model,
    params: &HashMap<String, SecretString>,
    rt: Arc<Runtime>,
) -> Result<Box<dyn Chat>, LlmError> {
    let model = construct_model(component, params).await?;

    // Handle tool usage
    let spice_tool_opt: Option<SpiceToolsOptions> = extract_secret!(params, "tools")
        .or(extract_secret!(params, "spice_tools"))
        .map(str::parse)
        .transpose()
        .map_err(|_| unreachable!("SpiceToolsOptions::from_str has no error condition"))?;

    let spice_recursion_limit: Option<usize> = extract_secret!(params, "tool_recursion_limit")
        .map(|x| {
            x.parse().map_err(|e| LlmError::FailedToLoadModel {
                source: format!(
                    "Invalid value specified for `params.recursion_depth`: {x}. Error: {e}"
                )
                .into(),
            })
        })
        .transpose()?;

    let tool_model = match spice_tool_opt {
        Some(opts) if opts.can_use_tools() => Box::new(ToolUsingChat::new(
            Arc::new(model),
            Arc::clone(&rt),
            get_tools(Arc::clone(&rt), &opts).await,
            spice_recursion_limit,
        )),
        Some(_) | None => model,
    };
    Ok(tool_model)
}

pub async fn construct_model(
    component: &spicepod::component::model::Model,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let model_id = component.get_model_id();
    let prefix = component.get_source().ok_or(LlmError::UnknownModelSource {
        from: component.from.clone(),
    })?;

    let model = match prefix {
        ModelSource::HuggingFace => huggingface(model_id, component, params).await,
        ModelSource::File => file(component, params),
        ModelSource::Anthropic => anthropic(model_id.as_deref(), params),
        ModelSource::Azure => azure(model_id, component.name.as_str(), params),
        ModelSource::Xai => xai(model_id.as_deref(), params),
        ModelSource::OpenAi => openai(model_id, params),
        ModelSource::SpiceAI => Err(LlmError::UnsupportedTaskForModel {
            from: "spiceai".into(),
            task: "llm".into(),
        }),
    }?;

    // Handle runtime wrapping
    let system_prompt = component
        .params
        .get("system_prompt")
        .cloned()
        .map(|s| s.to_string());
    let wrapper = ChatWrapper::new(
        model,
        component.name.as_str(),
        system_prompt,
        component.get_openai_request_overrides(),
    );
    Ok(Box::new(wrapper))
}

fn xai(
    model_id: Option<&str>,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let Some(api_key) = extract_secret!(params, "xai_api_key") else {
        return Err(LlmError::FailedToLoadModel {
            source: "No `xai_api_key` provided for xAI model.".into(),
        });
    };
    Ok(Box::new(Xai::new(model_id, api_key)) as Box<dyn Chat>)
}

fn anthropic(
    model_id: Option<&str>,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let api_base = extract_secret!(params, "endpoint");
    let api_key = extract_secret!(params, "anthropic_api_key");
    let auth_token = extract_secret!(params, "anthropic_auth_token");

    if api_key.is_none() && auth_token.is_none() {
        return Err(LlmError::FailedToLoadModel {
            source: "One of following `model.params` is required: `anthropic_api_key` or `anthropic_auth_token`.".into(),
        });
    }

    let cfg = AnthropicConfig::default()
        .with_api_key(api_key)
        .with_auth_token(auth_token)
        .with_base_url(api_base);

    let anthropic = Anthropic::new(cfg, model_id).map_err(|_| LlmError::FailedToLoadModel {
        source: format!("Unknown anthropic model: {:?}", model_id.clone()).into(),
    })?;

    Ok(Box::new(anthropic) as Box<dyn Chat>)
}

async fn huggingface(
    model_id: Option<String>,
    component: &spicepod::component::model::Model,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let Some(id) = model_id else {
        return Err(LlmError::FailedToLoadModel {
            source: "No model id for Huggingface model".to_string().into(),
        });
    };
    let model_type = extract_secret!(params, "model_type");
    let hf_token = params.get("hf_token");

    // For GGUF models, we require user specify via `.files[].path`
    let gguf_path = component
        .find_all_file_path(ModelFileType::Weights)
        .iter()
        .find_map(|p| {
            let path = PathBuf::from_str(p.as_str());
            if let Ok(Some(ext)) = path.as_ref().map(|pp| pp.extension()) {
                if ext.eq_ignore_ascii_case("gguf") {
                    return PathBuf::from_str(p.as_str()).ok();
                };
            }
            None
        });

    if let Some(path) = gguf_path {
        tracing::debug!(
            "For Huggingface model {}, the GGUF model {} will be downloaded and used.",
            component.name,
            path.display()
        );
        return llms::chat::create_hf_w_gguf(id.as_str(), path.as_path(), hf_token).await;
    }

    llms::chat::create_hf_model(&id, model_type, hf_token)
}

fn openai(
    model_id: Option<String>,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let api_base = extract_secret!(params, "endpoint");
    let api_key = extract_secret!(params, "openai_api_key");
    let org_id = extract_secret!(params, "openai_org_id");
    let project_id = extract_secret!(params, "openai_project_id");

    if let Some(temperature_str) = extract_secret!(params, "openai_temperature") {
        match temperature_str.parse::<f64>() {
            Ok(temperature) => {
                if temperature < 0.0 {
                    return Err(LlmError::InvalidParamError {
                        param: "openai_temperature".to_string(),
                        message: "Ensure it is a non-negative number.".to_string(),
                    });
                }
            }
            Err(_) => {
                return Err(LlmError::InvalidParamError {
                    param: "openai_temperature".to_string(),
                    message: "Ensure it is a non-negative number.".to_string(),
                })
            }
        }
    }

    Ok(Box::new(llms::openai::new_openai_client(
        model_id.unwrap_or(DEFAULT_LLM_MODEL.to_string()),
        api_base,
        api_key,
        org_id,
        project_id,
    )) as Box<dyn Chat>)
}

fn azure(
    model_id: Option<String>,
    model_name: &str,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let Some(model_name) = model_id else {
        return Err(LlmError::FailedToLoadModel {
            source: format!(
    "Azure model '{model_name}' requires a model ID in the format `from:azure:<model_id>`. See https://spiceai.org/docs/components/models/azure for details."
).into(),
        });
    };
    let api_base = extract_secret!(params, "endpoint");
    let api_version = extract_secret!(params, "azure_api_version");
    let deployment_name = extract_secret!(params, "azure_deployment_name");
    let api_key = extract_secret!(params, "azure_api_key");
    let entra_token = extract_secret!(params, "azure_entra_token");

    if api_base.is_none() {
        return Err(LlmError::FailedToLoadModel {
            source: format!(
    "Azure model '{model_name}' requires the 'endpoint' parameter. See https://spiceai.org/docs/components/models/azure for details."
).into(),
        });
    }

    if api_key.is_some() && entra_token.is_some() {
        return Err(LlmError::FailedToLoadModel {
            source: format!(
                "Azure model '{model_name}' allows only one of 'azure_api_key' or 'azure_entra_token'. See https://spiceai.org/docs/components/models/azure for details."
            )
            .into(),
        });
    }

    if api_key.is_none() && entra_token.is_none() {
        return Err(LlmError::FailedToLoadModel {
            source: format!(
                "Azure model '{model_name}' requires either 'azure_api_key' or 'azure_entra_token'. See https://spiceai.org/docs/components/models/azure for details."
            )
            .into(),
        });
    }

    Ok(Box::new(llms::openai::new_azure_client(
        model_name,
        api_base,
        api_version,
        deployment_name,
        entra_token,
        api_key,
    )) as Box<dyn Chat>)
}

fn file(
    component: &spicepod::component::model::Model,
    params: &HashMap<String, SecretString>,
) -> Result<Box<dyn Chat>, LlmError> {
    let model_weights = component.find_all_file_path(ModelFileType::Weights);
    if model_weights.is_empty() {
        return Err(LlmError::FailedToLoadModel {
            source: "No 'weights_path' parameter provided".into(),
        });
    }

    let tokenizer_path = component.find_any_file_path(ModelFileType::Tokenizer);
    let tokenizer_config_path = component.find_any_file_path(ModelFileType::TokenizerConfig);
    let config_path = component.find_any_file_path(ModelFileType::Config);
    let chat_template_literal = params
        .get("chat_template")
        .map(|s| s.expose_secret().as_str());

    llms::chat::create_local_model(
        model_weights.as_slice(),
        config_path.as_deref(),
        tokenizer_path.as_deref(),
        tokenizer_config_path.as_deref(),
        chat_template_literal,
    )
}

/// Wraps [`Chat`] models with additional handling specifically for the spice runtime (e.g. telemetry, injecting system prompts).
pub struct ChatWrapper {
    pub public_name: String,
    pub chat: Box<dyn Chat>,
    pub system_prompt: Option<String>,
    pub defaults: Vec<(String, serde_json::Value)>,
}
impl ChatWrapper {
    pub fn new(
        chat: Box<dyn Chat>,
        public_name: &str,
        system_prompt: Option<String>,
        defaults: Vec<(String, serde_json::Value)>,
    ) -> Self {
        Self {
            public_name: public_name.to_string(),
            chat,
            system_prompt,
            defaults,
        }
    }

    fn prepare_req(
        &self,
        req: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionRequest, OpenAIError> {
        let mut prepared_req = self.with_system_prompt(req)?;

        prepared_req = self.with_model_defaults(prepared_req);
        prepared_req = Self::with_stream_usage(prepared_req);
        Ok(prepared_req)
    }

    /// Injects a system prompt as the first message in the request, if it exists.
    fn with_system_prompt(
        &self,
        mut req: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionRequest, OpenAIError> {
        if let Some(prompt) = self.system_prompt.clone() {
            let system_message = ChatCompletionRequestSystemMessageArgs::default()
                .content(prompt)
                .build()?;
            req.messages
                .insert(0, ChatCompletionRequestMessage::System(system_message));
        }
        Ok(req)
    }

    /// Ensure that streaming requests have `stream_options: {"include_usage": true}` internally.
    fn with_stream_usage(mut req: CreateChatCompletionRequest) -> CreateChatCompletionRequest {
        if req.stream.is_some_and(|s| s) {
            req.stream_options = match req.stream_options {
                Some(mut opts) => {
                    opts.include_usage = true;
                    Some(opts)
                }
                None => Some(ChatCompletionStreamOptions {
                    include_usage: true,
                }),
            };
        }
        req
    }

    /// For [`None`] valued fields in a [`CreateChatCompletionRequest`], if the chat model has non-`None` defaults, use those instead.
    fn with_model_defaults(
        &self,
        mut req: CreateChatCompletionRequest,
    ) -> CreateChatCompletionRequest {
        for (key, v) in &self.defaults {
            let value = v.clone();
            match key.as_str() {
                "frequency_penalty" => {
                    req.frequency_penalty = req
                        .frequency_penalty
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "logit_bias" => {
                    req.logit_bias = req
                        .logit_bias
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "logprobs" => {
                    req.logprobs = req.logprobs.or_else(|| serde_json::from_value(value).ok());
                }
                "top_logprobs" => {
                    req.top_logprobs = req
                        .top_logprobs
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "max_completion_tokens" => {
                    req.max_completion_tokens = req
                        .max_completion_tokens
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "store" => {
                    req.store = req.store.or_else(|| serde_json::from_value(value).ok());
                }
                "metadata" => {
                    req.metadata = req.metadata.or_else(|| serde_json::from_value(value).ok());
                }
                "n" => req.n = req.n.or_else(|| serde_json::from_value(value).ok()),
                "presence_penalty" => {
                    req.presence_penalty = req
                        .presence_penalty
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "response_format" => {
                    req.response_format = req
                        .response_format
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "seed" => req.seed = req.seed.or_else(|| serde_json::from_value(value).ok()),
                "stop" => req.stop = req.stop.or_else(|| serde_json::from_value(value).ok()),
                "stream" => req.stream = req.stream.or_else(|| serde_json::from_value(value).ok()),
                "stream_options" => {
                    req.stream_options = req
                        .stream_options
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "temperature" => {
                    req.temperature = req
                        .temperature
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "top_p" => req.top_p = req.top_p.or_else(|| serde_json::from_value(value).ok()),
                "tools" => req.tools = req.tools.or_else(|| serde_json::from_value(value).ok()),
                "tool_choice" => {
                    req.tool_choice = req
                        .tool_choice
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "parallel_tool_calls" => {
                    req.parallel_tool_calls = req
                        .parallel_tool_calls
                        .or_else(|| serde_json::from_value(value).ok());
                }
                "user" => req.user = req.user.or_else(|| serde_json::from_value(value).ok()),
                _ => {
                    tracing::debug!("Ignoring unknown default key: {}", key);
                }
            };
        }
        req
    }
}

#[async_trait]
impl Chat for ChatWrapper {
    /// Expect `captured_output` to be instrumented by the underlying chat model (to not reopen/parse streams). i.e.
    /// ```rust
    /// tracing::info!(target: "task_history", captured_output = %chat_output)
    /// ```
    async fn chat_stream(
        &self,
        req: CreateChatCompletionRequest,
    ) -> Result<ChatCompletionResponseStream, OpenAIError> {
        let start = Instant::now();
        let span = tracing::span!(target: "task_history", tracing::Level::INFO, "ai_completion", stream=true, model = %req.model, input = %serde_json::to_string(&req).unwrap_or_default());
        let req = self.prepare_req(req)?;

        if let Some(metadata) = &req.metadata {
            tracing::info!(target: "task_history", metadata = %metadata);
        }

        let labels = request_labels(&req);
        match self.chat.chat_stream(req).instrument(span.clone()).await {
            Ok(resp) => {
                let public_name = self.public_name.clone();
                let stream_span = span.clone();
                let logged_stream = resp.map_ok(move |mut r| {r.model.clone_from(&public_name); r}).inspect(move |item| {
                    if let Ok(item) = item {

                        // not incremental; provider only emits usage on last chunk.
                        if let Some(usage) = item.usage.clone() {
                            tracing::info!(target: "task_history", parent: &stream_span.clone(), completion_tokens = %usage.completion_tokens, total_tokens = %usage.total_tokens, prompt_tokens = %usage.prompt_tokens, "labels");
                            handle_metrics(start.elapsed(), false, &labels);
                        }
                    }
                }).instrument(span.clone());
                Ok(Box::pin(logged_stream))
            }
            Err(e) => {
                tracing::error!(target: "task_history", parent: &span, "Failed to run chat model: {}", e);
                handle_metrics(start.elapsed(), true, &labels);
                Err(e)
            }
        }
    }

    async fn health(&self) -> ChatResult<()> {
        self.chat.health().await
    }

    /// Unlike [`ChatWrapper::chat_stream`], this method will instrument the `captured_output` for the model output.
    async fn chat_request(
        &self,
        req: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, OpenAIError> {
        let start = Instant::now();

        let span = tracing::span!(target: "task_history", tracing::Level::INFO, "ai_completion", stream=false, model = %req.model, input = %serde_json::to_string(&req).unwrap_or_default());
        let req = self.prepare_req(req)?;

        let labels = request_labels(&req);
        if let Some(metadata) = &req.metadata {
            tracing::info!(target: "task_history", parent: &span, metadata = %metadata, "labels");
        }

        let result = match self.chat.chat_request(req).instrument(span.clone()).await {
            Ok(mut resp) => {
                if let Some(usage) = resp.usage.clone() {
                    tracing::info!(target: "task_history", parent: &span, completion_tokens = %usage.completion_tokens, total_tokens = %usage.total_tokens, prompt_tokens = %usage.prompt_tokens, "labels");
                };
                let captured_output: Vec<_> = resp.choices.iter().map(|c| &c.message).collect();
                match serde_json::to_string(&captured_output) {
                    Ok(output) => {
                        tracing::info!(target: "task_history", parent: &span, captured_output = %output);
                    }
                    Err(e) => tracing::error!("Failed to serialize truncated output: {e}"),
                }
                resp.model.clone_from(&self.public_name);
                Ok(resp)
            }
            Err(e) => {
                tracing::error!(target: "task_history", parent: &span, "Failed to run chat model: {}", e);
                Err(e)
            }
        };
        handle_metrics(start.elapsed(), result.is_err(), &labels);
        result
    }

    async fn run(&self, prompt: String) -> ChatResult<Option<String>> {
        self.chat.run(prompt).await
    }

    async fn stream<'a>(
        &self,
        prompt: String,
    ) -> ChatResult<Pin<Box<dyn Stream<Item = ChatResult<Option<String>>> + Send>>> {
        self.chat.stream(prompt).await
    }

    fn as_sql(&self) -> Option<&dyn SqlGeneration> {
        self.chat.as_sql()
    }
}
