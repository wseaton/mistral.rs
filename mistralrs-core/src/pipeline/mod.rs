mod amoe;
mod cache_manager;
pub mod chat_template;
mod ggml;
mod gguf;
mod inputs_processor;
mod isq;
mod loaders;
mod macros;
mod normal;
mod paths;
mod processing;
mod sampling;
mod speculative;
mod vision;
use crate::aici::toktree::TokTrie;
use crate::amoe::{AnyMoeConfig, AnyMoeExpertType, AnyMoeTrainingInputs, AnyMoeTrainingResult};
use crate::paged_attention::{CacheConfig, CacheEngine};
use crate::prefix_cacher::PrefixCacheManager;
pub use amoe::{AnyMoeLoader, AnyMoePipeline};
use chat_template::ChatTemplate;
pub use ggml::{GGMLLoader, GGMLLoaderBuilder, GGMLSpecificConfig};
pub use gguf::{GGUFLoader, GGUFLoaderBuilder};
pub use inputs_processor::InputProcessorOutput;
pub use isq::{parse_isq_value, IsqModel};
pub use loaders::{
    AdapterKind, Gemma2Loader, GemmaLoader, Idefics2Loader, LLaVALoader, LLaVANextLoader,
    LlamaLoader, Loader, LocalModelPaths, MistralLoader, MixtralLoader, ModelKind, ModelPaths,
    NormalLoaderType, NormalLoadingMetadata, NormalModel, NormalModelLoader, Phi2Loader,
    Phi3Loader, Phi3RopeScaling, Phi3VLoader, PrettyName, QuantizationKind, Qwen2Loader,
    Starcoder2Loader, TokenSource, VisionLoaderType, VisionModel, VisionModelLoader,
};
use mistralrs_quant::IsqType;
pub use normal::{NormalLoader, NormalLoaderBuilder, NormalSpecificConfig};
pub(crate) use paths::{get_chat_template, get_model_paths, get_xlora_paths, XLoraPaths};
pub(crate) use processing::{
    apply_chat_template, BasicProcessor, MessagesAction, Processor, ProcessorCreator,
};
use rand_isaac::Isaac64Rng;
pub use speculative::{SpeculativeConfig, SpeculativeLoader, SpeculativePipeline};
use std::any::Any;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokenizers::Tokenizer;
pub use vision::{VisionLoader, VisionLoaderBuilder, VisionSpecificConfig};

use anyhow::Result;
use candle_core::{DType, Device, IndexOp, Tensor, Var};

use crate::sequence::Sequence;

pub use self::cache_manager::{Cache, CacheManager, LayerCaches};
pub use self::inputs_processor::{
    text_models_inputs_processor, InputsProcessor, InputsProcessorType,
};
use self::text_models_inputs_processor::PagedAttentionMeta;

pub struct GeneralMetadata {
    pub max_seq_len: usize,
    pub tok_trie: Arc<TokTrie>,
    pub has_no_kv_cache: bool,
    pub num_hidden_layers: usize,
    pub eos_tok: Vec<u32>,
    pub kind: ModelKind,
    // TODO: Replace is_xlora queries to check via kind instead:
    pub is_xlora: bool,
    pub activation_dtype: DType,
    pub sliding_window: Option<usize>,
    // PagedAttention stuff
    pub cache_config: Option<CacheConfig>,
    pub cache_engine: Option<CacheEngine>,
    pub prompt_batchsize: Option<NonZeroUsize>,
}

pub enum AdapterInstruction {
    Activate(Vec<String>),
    None,
}

pub enum CacheInstruction {
    In(AdapterInstruction),
    Out,
    Reset {
        reset_non_granular: bool,
        adapter_inst: AdapterInstruction,
    },
    Nothing(AdapterInstruction),
}

pub trait PreProcessingMixin: MetadataMixin {
    fn get_processor(&self) -> Arc<dyn Processor> {
        Arc::new(BasicProcessor)
    }
    fn get_chat_template(&self) -> Arc<ChatTemplate>;
    fn get_input_processor_config(&self) -> Option<Arc<dyn Any>>;
}

pub trait IsqPipelineMixin {
    fn re_isq_model(&mut self, dtype: IsqType) -> Result<()>;
}

pub trait CacheManagerMixin {
    /// Clone the cache FROM the sequences' cache TO the model cache. Only called for completion seqs.
    /// It is not a guarantee that this will be called for each completion step.
    fn clone_in_cache(&self, seqs: &mut [&mut Sequence], modify_draft_cache: bool);
    /// Clone the cache FROM the model cache TO the sequences. Called for prompt and completion seqs.
    /// It is not a guarantee that this will be called for each step.
    fn clone_out_cache(&self, seqs: &mut [&mut Sequence], modify_draft_cache: bool);
    /// Set the model cache to all None. Only called for prompt seqs.
    /// It is not a guarantee that this will be called for each prompt step.
    /// This may also reset the non granular state if applicable.
    fn set_none_cache(&self, reset_non_granular: bool, modify_draft_cache: bool);
    fn cache(&self) -> &Cache;
}

pub trait AdapterActivationMixin {
    /// Returns the number of activated adapters.
    fn activate_adapters(&mut self, adapters: Vec<String>) -> Result<usize>;
}

pub trait MetadataMixin {
    fn device(&self) -> Device;
    fn tokenizer(&self) -> Arc<Tokenizer>;
    fn name(&self) -> String;
    fn reset_non_granular_state(&self);
    fn get_metadata(&self) -> Arc<GeneralMetadata>;
}

/// Implemented by the base model of an AnyMoe.
pub trait AnyMoePipelineMixin {
    /// Get vars for each gating layer
    fn amoe_layer_vars(&self) -> Vec<Vec<Var>> {
        unreachable!()
    }
    fn amoe_finish_training(&mut self, _gate_model_id: Option<String>) -> candle_core::Result<()> {
        unreachable!()
    }
    fn amoe_base_model_trainable_params(&self) -> usize {
        unreachable!()
    }
    fn amoe_supported(&self) -> bool {
        false
    }
    /// Per-layer cached outputs.
    fn amoe_take_cached_gating_outputs(&mut self) -> Vec<Tensor> {
        unreachable!()
    }
    /// Inject the MoE layers
    #[allow(clippy::too_many_arguments)]
    fn amoe_create_layers(
        &mut self,
        _model_ids: Vec<String>,
        _token: &TokenSource,
        _revision: Option<String>,
        _match_regex: &str,
        _config: AnyMoeConfig,
        _dtype: DType,
        _dev: &Device,
        (_prefix, _mlp): (String, String),
        _layers: Vec<usize>,
        _expert_type: AnyMoeExpertType,
        _silent: bool,
        _gate_model_id: Option<String>,
    ) -> candle_core::Result<()> {
        unreachable!()
    }
    /// Pre-train the gating layers
    #[allow(clippy::too_many_arguments)]
    fn amoe_pre_train(
        &self,
        _inputs: AnyMoeTrainingInputs,
        (_prefix, _mlp): (String, String),
        _model_ids: Vec<String>,
        _token: TokenSource,
        _revision: Option<String>,
        _layers: Vec<usize>,
        _silent: bool,
    ) -> Result<Option<AnyMoeTrainingResult>, candle_core::Error> {
        unreachable!()
    }
}

#[derive(PartialEq, Copy, Clone)]
pub enum ModelCategory {
    Text,
    Vision { has_conv2d: bool },
}

pub enum CacheBackendMetadata<'a> {
    DefaultInstructions {
        pre_op: CacheInstruction,
        post_op: CacheInstruction,
    },
    PagedAttention {
        metadata: PagedAttentionMeta<'a>,
        blocks_to_swap_in: HashMap<usize, usize>,
        blocks_to_swap_out: HashMap<usize, usize>,
        blocks_to_copy: HashMap<usize, Vec<usize>>,
    },
}

#[async_trait::async_trait]
pub trait Pipeline:
    Send
    + Sync
    + PreProcessingMixin
    + IsqPipelineMixin
    + CacheManagerMixin
    + AdapterActivationMixin
    + MetadataMixin
    + AnyMoePipelineMixin
{
    fn forward_inputs(&self, inputs: Box<dyn Any>) -> Result<Tensor, candle_core::Error>;

    #[allow(clippy::too_many_arguments)]
    async fn step(
        &mut self,
        input_seqs: &mut [&mut Sequence],
        is_prompt: bool,
        prefix_cacher: &mut PrefixCacheManager,
        disable_eos_stop: bool,
        rng: Arc<std::sync::Mutex<Isaac64Rng>>,
        backend_metadata: CacheBackendMetadata<'_>,
    ) -> Result<(), candle_core::Error> {
        match backend_metadata {
            CacheBackendMetadata::DefaultInstructions { pre_op, post_op } => {
                let inputs_iter = self.get_processor().inputs_processor().process_inputs(
                    self.tokenizer(),
                    input_seqs,
                    is_prompt,
                    self.get_metadata().is_xlora,
                    &self.device(),
                    self.get_metadata().has_no_kv_cache,
                    None,
                    self.get_input_processor_config(),
                    None,
                    self.get_metadata().prompt_batchsize,
                );

                let mut logits = vec![None; input_seqs.len()];

                for (i, inputs) in inputs_iter.enumerate() {
                    let InputProcessorOutput {
                        inputs,
                        seq_indices,
                    } = inputs.map_err(|e| candle_core::Error::Msg(e.to_string()))?;
                    if i == 0 {
                        match pre_op {
                            CacheInstruction::In(ref adapter_inst) => {
                                match adapter_inst {
                                    AdapterInstruction::Activate(adapters) => {
                                        self.activate_adapters(adapters.clone()).map_err(|e| {
                                            candle_core::Error::msg(<anyhow::Error as AsRef<
                                                dyn std::error::Error,
                                            >>::as_ref(
                                                &e
                                            ))
                                        })?
                                    }
                                    AdapterInstruction::None => 0,
                                };
                                self.clone_in_cache(input_seqs, false)
                            }
                            CacheInstruction::Nothing(ref adapter_inst) => {
                                match adapter_inst {
                                    AdapterInstruction::Activate(adapters) => {
                                        self.activate_adapters(adapters.clone()).map_err(|e| {
                                            candle_core::Error::msg(<anyhow::Error as AsRef<
                                                dyn std::error::Error,
                                            >>::as_ref(
                                                &e
                                            ))
                                        })?
                                    }
                                    AdapterInstruction::None => 0,
                                };
                            }
                            CacheInstruction::Reset {
                                reset_non_granular,
                                ref adapter_inst,
                            } => {
                                match adapter_inst {
                                    AdapterInstruction::Activate(adapters) => {
                                        self.activate_adapters(adapters.clone()).map_err(|e| {
                                            candle_core::Error::msg(<anyhow::Error as AsRef<
                                                dyn std::error::Error,
                                            >>::as_ref(
                                                &e
                                            ))
                                        })?
                                    }
                                    AdapterInstruction::None => 0,
                                };
                                self.set_none_cache(reset_non_granular, false)
                            }
                            _ => unreachable!("Unreachable PRE cache op."),
                        }
                    }

                    let raw_logits = self.forward_inputs(inputs)?;

                    for (logit_idx, seq_idx) in seq_indices.into_iter().enumerate() {
                        logits[seq_idx] = Some(raw_logits.i(logit_idx)?);
                    }
                }

                let logits = logits
                    .into_iter()
                    .map(|l| {
                        l.expect("Did not get any inputs. This is shocking.")
                            .to_device(&Device::Cpu)
                    })
                    .collect::<candle_core::Result<Vec<_>>>()?;

                match post_op {
                    CacheInstruction::Out => self.clone_out_cache(input_seqs, false),
                    CacheInstruction::Nothing(_) => (),
                    CacheInstruction::Reset {
                        reset_non_granular,
                        adapter_inst: _,
                    } => self.set_none_cache(reset_non_granular, false),
                    _ => unreachable!("Unreachable POST cache op."),
                }

                self.sample(input_seqs, logits, prefix_cacher, disable_eos_stop, rng)
                    .await?;
                Ok(())
            }
            CacheBackendMetadata::PagedAttention {
                metadata,
                blocks_to_copy,
                blocks_to_swap_in,
                blocks_to_swap_out,
            } => {
                self.get_metadata()
                    .cache_engine
                    .as_ref()
                    .expect("PagedAttention must have cache engine.")
                    .execute_scheduler_ops(blocks_to_swap_in, blocks_to_swap_out, blocks_to_copy)?;

                let inputs_iter = self.get_processor().inputs_processor().process_inputs(
                    self.tokenizer(),
                    input_seqs,
                    is_prompt,
                    self.get_metadata().is_xlora,
                    &self.device(),
                    self.get_metadata().has_no_kv_cache,
                    None,
                    self.get_input_processor_config(),
                    Some(metadata),
                    self.get_metadata().prompt_batchsize,
                );

                let mut logits = vec![None; input_seqs.len()];

                for inputs in inputs_iter {
                    let InputProcessorOutput {
                        inputs,
                        seq_indices,
                    } = inputs.map_err(|e| candle_core::Error::Msg(e.to_string()))?;

                    let raw_logits = self.forward_inputs(inputs)?;

                    for (logit_idx, seq_idx) in seq_indices.into_iter().enumerate() {
                        logits[seq_idx] = Some(raw_logits.i(logit_idx)?);
                    }
                }

                let logits = logits
                    .into_iter()
                    .map(|l| {
                        l.expect("Did not get any inputs. This is shocking.")
                            .to_device(&Device::Cpu)
                    })
                    .collect::<candle_core::Result<Vec<_>>>()?;

                self.sample(input_seqs, logits, prefix_cacher, disable_eos_stop, rng)
                    .await?;
                Ok(())
            }
        }
    }

    async fn sample(
        &self,
        seqs: &mut [&mut Sequence],
        logits: Vec<Tensor>,
        prefix_cacher: &mut PrefixCacheManager,
        disable_eos_stop: bool,
        rng: Arc<std::sync::Mutex<Isaac64Rng>>,
    ) -> Result<(), candle_core::Error>;

    fn category(&self) -> ModelCategory;
}

pub(crate) fn extract_logits(
    logits: &Tensor,
    context_lens: Vec<(usize, usize)>,
) -> candle_core::Result<Tensor> {
    let mut toks = Vec::new();
    for (dim, (start, len)) in logits.chunk(logits.dims()[0], 0)?.iter().zip(context_lens) {
        toks.push(dim.narrow(1, start, len)?);
    }
    Tensor::cat(&toks, 0)
}

#[cfg(test)]
mod tests {
    use crate::MessageContent;
    use either::Either;
    use indexmap::IndexMap;

    macro_rules! hashmap {
        (@single $($x:tt)*) => (());
        (@count $($rest:expr),*) => (<[()]>::len(&[$(hashmap!(@single $rest)),*]));

        ($($key:expr => $value:expr,)+) => { hashmap!($($key => $value),+) };
        ($($key:expr => $value:expr),*) => {
            {
                let _cap = hashmap!(@count $($key),*);
                let mut _map = ::indexmap::IndexMap::with_capacity(_cap);
                $(
                    let _ = _map.insert($key, $value);
                )*
                _map
            }
        };
    }

    #[cfg(test)]
    #[track_caller]
    fn test_with_inputs(
        templates: &[(bool, &str, &str, &str, &str)],
        expected_outputs: &[&str],
        inputs: Vec<IndexMap<String, MessageContent>>,
    ) {
        use crate::pipeline::chat_template::ChatTemplateValue;

        use super::chat_template::apply_chat_template_to;
        let mut failed = Vec::new();
        let n_templates = templates.len();
        for ((has_system, bos, eos, unk, template), expected) in
            templates.iter().zip(expected_outputs)
        {
            let output = match apply_chat_template_to(
                if !has_system {
                    inputs[1..].to_vec()
                } else {
                    inputs.clone()
                },
                true,
                &ChatTemplateValue(Either::Left(template.to_string())),
                Some(bos.to_string()),
                Some(eos.to_string()),
                Some(unk.to_string()),
                Vec::new(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    failed.push(format!("Failed with {e}."));
                    continue;
                }
            };
            if output != *expected {
                failed.push(format!(
                    "Expected: `{}` \n\nGot:      `{}`",
                    expected.replace('\n', "\\n"),
                    output.replace('\n', "\\n")
                ));
            }
        }
        if !failed.is_empty() {
            for (i, line) in failed.iter().enumerate() {
                println!("------------ Template {i} ------------");
                println!("{line}");
            }
            println!("------------------------");
            panic!("{}/{n_templates} chat templates failed.", failed.len());
        }
    }

    #[test]
    /// Generating these cases:
    /// ```py
    /// >>> t=transformers.AutoTokenizer.from_pretrained(...)
    /// # If non-system prompt model
    /// >>> t.apply_chat_template([{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there"},{"role":"user","content":"Who are you"},{"role":"assistant","content":"   I am an assistant   "},{"role":"user","content":"Another question"}], add_generation_prompt=True, tokenize=False)
    /// # If system prompt model
    /// >>> t.apply_chat_template([{"role":"system","content":"You are a helpful assistant"},{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there"},{"role":"user","content":"Who are you"},{"role":"assistant","content":"   I am an assistant   "},{"role":"user","content":"Another question"}], add_generation_prompt=True, tokenize=False)
    /// ```
    fn test_chat_templates() {
        let templates = [
            // ChatML: https://huggingface.co/teknium/OpenHermes-2.5-Mistral-7B
            (true, "<s>", "</s>", "<unk>", "{% for message in messages %}{{'<|im_start|>' + message['role'] + '\n' + message['content'] + '<|im_end|>' + '\n'}}{% endfor %}{% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}"),
            // mistralai/Mistral-7B-Instruct-v0.1
            (false, "<s>", "</s>", "<unk>", "{{ bos_token }}{% for message in messages %}{% if (message['role'] == 'user') != (loop.index0 % 2 == 0) %}{{ raise_exception('Conversation roles must alternate user/assistant/user/assistant/...') }}{% endif %}{% if message['role'] == 'user' %}{{ '[INST] ' + message['content'] + ' [/INST]' }}{% elif message['role'] == 'assistant' %}{{ message['content'] + eos_token + ' ' }}{% else %}{{ raise_exception('Only user and assistant roles are supported!') }}{% endif %}{% endfor %}"),
            // meta-llama/Llama-2-13b-chat-hf
            (true, "<s>", "</s>", "<unk>", "{% if messages[0]['role'] == 'system' %}{% set loop_messages = messages[1:] %}{% set system_message = messages[0]['content'] %}{% else %}{% set loop_messages = messages %}{% set system_message = false %}{% endif %}{% for message in loop_messages %}{% if (message['role'] == 'user') != (loop.index0 % 2 == 0) %}{{ raise_exception('Conversation roles must alternate user/assistant/user/assistant/...') }}{% endif %}{% if loop.index0 == 0 and system_message != false %}{% set content = '<<SYS>>\\n' + system_message + '\\n<</SYS>>\\n\\n' + message['content'] %}{% else %}{% set content = message['content'] %}{% endif %}{% if message['role'] == 'user' %}{{ bos_token + '[INST] ' + content.strip() + ' [/INST]' }}{% elif message['role'] == 'assistant' %}{{ ' '  + content.strip() + ' ' + eos_token }}{% endif %}{% endfor %}"),
            // mistralai/Mixtral-8x7B-Instruct-v0.1
            (false, "<s>", "</s>", "<unk>", "{{ bos_token }}{% for message in messages %}{% if (message['role'] == 'user') != (loop.index0 % 2 == 0) %}{{ raise_exception('Conversation roles must alternate user/assistant/user/assistant/...') }}{% endif %}{% if message['role'] == 'user' %}{{ '[INST] ' + message['content'] + ' [/INST]' }}{% elif message['role'] == 'assistant' %}{{ message['content'] + eos_token}}{% else %}{{ raise_exception('Only user and assistant roles are supported!') }}{% endif %}{% endfor %}"),
            // google/gemma-7b-it
            (false, "<bos>", "<eos>", "<unk>", "{{ bos_token }}{% if messages[0]['role'] == 'system' %}{{ raise_exception('System role not supported') }}{% endif %}{% for message in messages %}{% if (message['role'] == 'user') != (loop.index0 % 2 == 0) %}{{ raise_exception('Conversation roles must alternate user/assistant/user/assistant/...') }}{% endif %}{% if (message['role'] == 'assistant') %}{% set role = 'model' %}{% else %}{% set role = message['role'] %}{% endif %}{{ '<start_of_turn>' + role + '\n' + message['content'] | trim + '<end_of_turn>\n' }}{% endfor %}{% if add_generation_prompt %}{{'<start_of_turn>model\n'}}{% endif %}"),
            // HuggingFaceM4/idefics2-8b-chatty
            (true, "<s>", "</s>", "<unk>", "{% for message in messages %}{{message['role'].capitalize()}}{% if message['content'][0]['type'] == 'image' %}{{':'}}{% else %}{{': '}}{% endif %}{% for line in message['content'] %}{% if line['type'] == 'text' %}{{line['text']}}{% elif line['type'] == 'image' %}{{ '<image>' }}{% endif %}{% endfor %}<end_of_utterance>\n{% endfor %}{% if add_generation_prompt %}{{ 'Assistant:' }}{% endif %}"),
        ];
        let expected_outputs = [
            // ChatML: https://huggingface.co/teknium/OpenHermes-2.5-Mistral-7B
            "<|im_start|>system\nYou are a helpful assistant<|im_end|>\n<|im_start|>user\nHello<|im_end|>\n<|im_start|>assistant\nHi there<|im_end|>\n<|im_start|>user\nWho are you<|im_end|>\n<|im_start|>assistant\n   I am an assistant   <|im_end|>\n<|im_start|>user\nAnother question<|im_end|>\n<|im_start|>assistant\n",
            // mistralai/Mistral-7B-Instruct-v0.1
            "<s>[INST] Hello [/INST]Hi there</s> [INST] Who are you [/INST]   I am an assistant   </s> [INST] Another question [/INST]",
            // meta-llama/Llama-2-13b-chat-hf
            "<s>[INST] <<SYS>>\nYou are a helpful assistant\n<</SYS>>\n\nHello [/INST] Hi there </s><s>[INST] Who are you [/INST] I am an assistant </s><s>[INST] Another question [/INST]",
            // mistralai/Mixtral-8x7B-Instruct-v0.1
            "<s>[INST] Hello [/INST]Hi there</s>[INST] Who are you [/INST]   I am an assistant   </s>[INST] Another question [/INST]",
            // google/gemma-7b-it
            "<bos><start_of_turn>user\nHello<end_of_turn>\n<start_of_turn>model\nHi there<end_of_turn>\n<start_of_turn>user\nWho are you<end_of_turn>\n<start_of_turn>model\nI am an assistant<end_of_turn>\n<start_of_turn>user\nAnother question<end_of_turn>\n<start_of_turn>model\n",
        ];
        let messages = [
            ["system", "You are a helpful assistant"],
            ["user", "Hello"],
            ["assistant", "Hi there"],
            ["user", "Who are you"],
            ["assistant", "   I am an assistant   "],
            ["user", "Another question"],
        ];
        let mut inputs = Vec::new();
        for [role, content] in messages {
            let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
                IndexMap::new();
            message.insert("role".to_string(), Either::Left(role.to_string()));
            message.insert("content".to_string(), Either::Left(content.to_string()));
            inputs.push(message);
        }
        test_with_inputs(&templates, &expected_outputs, inputs);
    }

    #[test]
    /// Generating these cases:
    /// ```py
    /// >>> processor=transformers.AutoProcessor.from_pretrained(...)
    /// >>> processor.apply_chat_template([
    ///         {"role":"system","content":[{"type":"text", "text": "You are a helpful assistant"}]},
    ///         {"role":"user","content":[{"type":"image"}, {"type":"text", "text": "Hello, please describe the above."}]},
    ///         {"role":"assistant","content":[{"type":"text", "text": "Hi there"}]},
    ///         {"role":"user","content":[{"type":"text", "text": "Who are you"}]},
    ///         {"role":"assistant","content":[{"type":"text", "text": "   I am an assistant   "}]},
    ///         {"role":"user","content":[{"type":"text", "text": "Another question"}]}
    ///     ], add_generation_prompt=True, tokenize=False)
    /// ```
    fn test_image_chat_templates() {
        let templates = [
            // HuggingFaceM4/idefics2-8b-chatty
            (true, "<s>", "</s>", "<unk>", "{% for message in messages %}{{message['role'].capitalize()}}{% if message['content'][0]['type'] == 'image' %}{{':'}}{% else %}{{': '}}{% endif %}{% for line in message['content'] %}{% if line['type'] == 'text' %}{{line['text']}}{% elif line['type'] == 'image' %}{{ '<image>' }}{% endif %}{% endfor %}<end_of_utterance>\n{% endfor %}{% if add_generation_prompt %}{{ 'Assistant:' }}{% endif %}"),
        ];
        let expected_outputs = [
            // HuggingFaceM4/idefics2-8b-chatty
            "System: You are a helpful assistant<end_of_utterance>\nUser:<image>Hello, please describe the above.<end_of_utterance>\nAssistant: Hi there<end_of_utterance>\nUser:<image>This is me, who are you<end_of_utterance>\nAssistant:    I am an assistant   <end_of_utterance>\nUser:<image>Another question, what is this?<end_of_utterance>\nAssistant:",
        ];

        let mut inputs = Vec::new();

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("system".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![hashmap! {
                "type".to_string() => "text".to_string(),
                "text".to_string() => "You are a helpful assistant".to_string()
            }]),
        );
        inputs.push(message);

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("user".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![
                hashmap! {
                    "type".to_string() => "image".to_string()
                },
                hashmap! {
                    "type".to_string() => "text".to_string(),
                    "text".to_string() => "Hello, please describe the above.".to_string()
                },
            ]),
        );
        inputs.push(message);

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("assistant".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![hashmap! {
                "type".to_string() => "text".to_string(),
                "text".to_string() => "Hi there".to_string()
            }]),
        );
        inputs.push(message);

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("user".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![
                hashmap! {
                    "type".to_string() => "image".to_string()
                },
                hashmap! {
                    "type".to_string() => "text".to_string(),
                    "text".to_string() => "This is me, who are you".to_string()
                },
            ]),
        );
        inputs.push(message);

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("assistant".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![hashmap! {
                "type".to_string() => "text".to_string(),
                "text".to_string() => "   I am an assistant   ".to_string()
            }]),
        );
        inputs.push(message);

        let mut message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        message.insert("role".to_string(), Either::Left("user".to_string()));
        message.insert(
            "content".to_string(),
            Either::Right(vec![
                hashmap! {
                    "type".to_string() => "image".to_string()
                },
                hashmap! {
                    "type".to_string() => "text".to_string(),
                    "text".to_string() => "Another question, what is this?".to_string()
                },
            ]),
        );
        inputs.push(message);

        test_with_inputs(&templates, &expected_outputs, inputs);
    }
}
