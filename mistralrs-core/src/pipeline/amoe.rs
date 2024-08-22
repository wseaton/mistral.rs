use std::{
    any::Any,
    fs::{self, File},
    io::Read,
    path::Path,
    sync::Arc,
};

use base64::{engine::general_purpose, Engine};
use candle_core::{DType, Device, Tensor};
use candle_nn::{AdamW, Optimizer, ParamsAdamW};
use either::Either;
use image::DynamicImage;
use indexmap::IndexMap;
use mistralrs_quant::IsqType;
#[cfg(feature = "plotly")]
use plotly::{layout::Axis, ImageFormat, Plot, Scatter};
use rand::{seq::SliceRandom, thread_rng};
use rand_isaac::Isaac64Rng;
use tracing::{info, warn};

use crate::{
    aici::toktree::TokTrie,
    amoe::{AnyMoeConfig, AnyMoeTrainingInputRow, AnyMoeTrainingInputs, AnyMoeTrainingResult},
    get_mut_arcmutex,
    prefix_cacher::PrefixCacheManager,
    sampler::Sampler,
    sequence::{Sequence, SequenceGroup, SequenceRecognizer},
    utils::progress::NiceProgressBar,
    DeviceMapMetadata, Loader, ModelCategory, ModelKind, ModelPaths, PagedAttentionConfig,
    Pipeline, Response, TokenSource, TryIntoDType,
};

use super::{
    AdapterActivationMixin, AnyMoePipelineMixin, CacheManagerMixin, IsqPipelineMixin,
    MetadataMixin, PreProcessingMixin,
};

pub struct AnyMoeLoader {
    pub target: Box<dyn Loader>,
    pub config: AnyMoeConfig,
    pub path: String,
    pub prefix: String,
    pub mlp: String,
    pub model_ids: Vec<String>,
    pub layers: Vec<usize>,
}

pub struct AnyMoePipeline {
    target: Arc<tokio::sync::Mutex<dyn Pipeline>>,
    config: AnyMoeConfig,
}

impl Loader for AnyMoeLoader {
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn load_model_from_hf(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
        dtype: &dyn TryIntoDType,
        device: &Device,
        silent: bool,
        mapper: DeviceMapMetadata,
        in_situ_quant: Option<IsqType>,
        paged_attn_config: Option<PagedAttentionConfig>,
    ) -> anyhow::Result<Arc<tokio::sync::Mutex<dyn Pipeline + Send + Sync>>> {
        let paged_attn_config = if paged_attn_config.is_none() {
            warn!("AnyMoE does not currently support PagedAttention, running without");
            None
        } else {
            paged_attn_config
        };

        let target = self.target.load_model_from_hf(
            revision.clone(),
            token_source.clone(),
            dtype,
            device,
            silent,
            mapper.clone(),
            in_situ_quant,
            paged_attn_config,
        )?;
        Ok(Arc::new(tokio::sync::Mutex::new(AnyMoePipeline::new(
            target,
            self.config.clone(),
            AnyMoeTrainingInputs::from_json(&self.path)?,
            self.prefix.clone(),
            self.mlp.clone(),
            self.model_ids.clone(),
            token_source,
            revision,
            self.layers.clone(),
            silent,
        )?)))
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn load_model_from_path(
        &self,
        paths: &Box<dyn ModelPaths>,
        dtype: &dyn TryIntoDType,
        device: &Device,
        silent: bool,
        mapper: DeviceMapMetadata,
        in_situ_quant: Option<IsqType>,
        paged_attn_config: Option<PagedAttentionConfig>,
    ) -> anyhow::Result<Arc<tokio::sync::Mutex<dyn Pipeline + Send + Sync>>> {
        let paged_attn_config = if paged_attn_config.is_none() {
            warn!("AnyMoE does not currently support PagedAttention, running without");
            None
        } else {
            paged_attn_config
        };

        let target = self.target.load_model_from_path(
            paths,
            dtype,
            device,
            silent,
            mapper.clone(),
            in_situ_quant,
            paged_attn_config,
        )?;
        Ok(Arc::new(tokio::sync::Mutex::new(AnyMoePipeline::new(
            target,
            self.config.clone(),
            AnyMoeTrainingInputs::from_json(&self.path)?,
            self.prefix.clone(),
            self.mlp.clone(),
            self.model_ids.clone(),
            TokenSource::None,
            None,
            self.layers.clone(),
            silent,
        )?)))
    }
    fn get_id(&self) -> String {
        format!("AnyMoE: tgt = `{}`", self.target.get_id(),)
    }
    fn get_kind(&self) -> ModelKind {
        ModelKind::AnyMoe {
            target: Box::new(self.target.get_kind()),
        }
    }
}

impl AnyMoePipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: Arc<tokio::sync::Mutex<dyn Pipeline>>,
        config: AnyMoeConfig,
        inputs: AnyMoeTrainingInputs,
        prefix: String,
        mlp: String,
        model_ids: Vec<String>,
        token: TokenSource,
        revision: Option<String>,
        layers: Vec<usize>,
        silent: bool,
    ) -> anyhow::Result<Self> {
        let this = Self { target, config };
        info!("Loaded pretraining dataset of {} samples.", inputs.len());
        match this.amoe_pre_train(
            inputs,
            (prefix, mlp),
            model_ids,
            token,
            revision,
            layers,
            silent,
        )? {
            Some(AnyMoeTrainingResult { steps, final_loss }) => {
                info!("Finished training in {steps} steps. Final losses per layer: {final_loss:?}")
            }
            None => {
                info!("Not training gating layer, using trained gating layer specified in config")
            }
        }
        Ok(this)
    }
}

impl AdapterActivationMixin for AnyMoePipeline {
    fn activate_adapters(&mut self, adapters: Vec<String>) -> anyhow::Result<usize> {
        get_mut_arcmutex!(self.target).activate_adapters(adapters)
    }
}

impl CacheManagerMixin for AnyMoePipeline {
    fn cache(&self) -> &super::Cache {
        unreachable!()
    }
    fn clone_in_cache(&self, seqs: &mut [&mut Sequence], modify_draft_cache: bool) {
        get_mut_arcmutex!(self.target).clone_in_cache(seqs, modify_draft_cache)
    }
    fn clone_out_cache(&self, seqs: &mut [&mut Sequence], modify_draft_cache: bool) {
        get_mut_arcmutex!(self.target).clone_out_cache(seqs, modify_draft_cache)
    }
    fn set_none_cache(&self, reset_non_granular: bool, modify_draft_cache: bool) {
        get_mut_arcmutex!(self.target).set_none_cache(reset_non_granular, modify_draft_cache)
    }
}

impl IsqPipelineMixin for AnyMoePipeline {
    fn re_isq_model(&mut self, dtype: IsqType) -> anyhow::Result<()> {
        get_mut_arcmutex!(self.target).re_isq_model(dtype)
    }
}

impl PreProcessingMixin for AnyMoePipeline {
    fn get_chat_template(&self) -> Arc<crate::ChatTemplate> {
        get_mut_arcmutex!(self.target).get_chat_template()
    }
    fn get_input_processor_config(&self) -> Option<Arc<dyn Any>> {
        get_mut_arcmutex!(self.target).get_input_processor_config()
    }
    fn get_processor(&self) -> Arc<dyn super::Processor> {
        get_mut_arcmutex!(self.target).get_processor()
    }
}

impl MetadataMixin for AnyMoePipeline {
    fn device(&self) -> Device {
        get_mut_arcmutex!(self.target).device()
    }
    fn get_metadata(&self) -> Arc<super::GeneralMetadata> {
        get_mut_arcmutex!(self.target).get_metadata()
    }
    fn name(&self) -> String {
        get_mut_arcmutex!(self.target).name()
    }
    fn reset_non_granular_state(&self) {
        get_mut_arcmutex!(self.target).reset_non_granular_state()
    }
    fn tokenizer(&self) -> Arc<tokenizers::Tokenizer> {
        get_mut_arcmutex!(self.target).tokenizer()
    }
}

#[async_trait::async_trait]
impl Pipeline for AnyMoePipeline {
    fn forward_inputs(&self, inputs: Box<dyn Any>) -> Result<Tensor, candle_core::Error> {
        get_mut_arcmutex!(self.target).forward_inputs(inputs)
    }

    async fn sample(
        &self,
        seqs: &mut [&mut Sequence],
        logits: Vec<Tensor>,
        prefix_cacher: &mut PrefixCacheManager,
        disable_eos_stop: bool,
        rng: Arc<std::sync::Mutex<Isaac64Rng>>,
    ) -> Result<(), candle_core::Error> {
        get_mut_arcmutex!(self.target)
            .sample(seqs, logits, prefix_cacher, disable_eos_stop, rng)
            .await
    }

    fn category(&self) -> ModelCategory {
        get_mut_arcmutex!(self.target).category()
    }
}

impl AnyMoePipelineMixin for AnyMoePipeline {
    // Training result is None if inference
    fn amoe_pre_train(
        &self,
        inputs: AnyMoeTrainingInputs,
        (prefix, mlp): (String, String),
        model_ids: Vec<String>,
        token: TokenSource,
        revision: Option<String>,
        layers: Vec<usize>,
        silent: bool,
    ) -> anyhow::Result<Option<AnyMoeTrainingResult>, candle_core::Error> {
        let mut target = get_mut_arcmutex!(self.target);
        if !target.amoe_supported() {
            candle_core::bail!("AnyMoE is not supported for this model.");
        }

        let device = target.device();
        let processor = target.get_processor();
        let inputs_processor = target.get_processor().inputs_processor();
        let tokenizer = target.tokenizer();
        let metadata = target.get_metadata().clone();
        let input_processor_cfg = target.get_input_processor_config().clone();

        let AnyMoeConfig {
            hidden_size: _,
            lr,
            epochs,
            batch_size,
            expert_type,
            gate_model_id,
            training,
            loss_svg,
        } = self.config.clone();
        let mut steps = 0;

        info!("Expert type: {expert_type:?}");
        info!("Expert model ids: {model_ids:?}");

        // Inject the AnyMoE layers
        target.amoe_create_layers(
            model_ids,
            &token,
            revision,
            &mlp.clone(),
            self.config.clone(),
            metadata.activation_dtype,
            &device,
            (prefix, mlp),
            layers,
            expert_type,
            silent,
            if !training {
                gate_model_id.clone()
            } else {
                None
            },
        )?;
        let layer_vars = target.amoe_layer_vars();

        // If there are no trainable params, assume we got a gate model id so no training
        if target.amoe_base_model_trainable_params() == 0 {
            return Ok(None);
        }

        info!(
            "{} gating layers, {} trainable parameters, lr = {lr}, {epochs} epochs, batch size = {batch_size}",
            layer_vars.len(),
            target.amoe_base_model_trainable_params()
        );

        let mut optimizers = layer_vars
            .into_iter()
            .map(|vars| {
                AdamW::new(
                    vars,
                    ParamsAdamW {
                        lr,
                        beta1: 0.9,
                        beta2: 0.999,
                        eps: 1e-8,
                        weight_decay: 0.0,
                    },
                )
            })
            .collect::<candle_core::Result<Vec<_>>>()?;

        let mut rng = thread_rng();
        let mut samples = inputs.into_inner();

        // Create several dummy objects for the sequences. No custom logits processors.
        let (dummy_sender, _) = tokio::sync::mpsc::channel(10000);
        let dummy_sampler =
            Sampler::new(None, 0, tokenizer.clone(), None, None, -1, 0.0, 0.0, vec![]);

        let dummy_group = Arc::new(tokio::sync::Mutex::new(SequenceGroup::new(
            1, false, false, 0,
        )));

        // Clear KV cache in prep for training
        target.set_none_cache(true, true);

        let mut latest_loss = vec![0.0; optimizers.len()];
        let mut all_losses = Vec::new();

        for _ in NiceProgressBar::<_, 'g'>(0..epochs, "Training gating layers") {
            samples.as_mut_slice().shuffle(&mut rng);
            for batch in samples.chunks(batch_size) {
                steps += 1;

                // === PREPARE INPUTS ==
                let mut seqs = Vec::new();
                for AnyMoeTrainingInputRow {
                    prompt,
                    expert: _,
                    image_urls,
                } in batch
                {
                    let tokens = processor
                        .process(
                            &*target,
                            vec![IndexMap::from([
                                ("role".to_string(), Either::Left("user".to_string())),
                                ("content".to_string(), Either::Left(prompt.clone())),
                            ])],
                            true,
                            Vec::new(),
                        )
                        .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
                    let images = image_urls.as_ref().map(|urls| {
                        urls.iter()
                            .map(|url| -> anyhow::Result<DynamicImage> {
                                let bytes = if url.contains("http") {
                                    // Read from http
                                    match reqwest::blocking::get(url.clone()) {
                                        Ok(http_resp) => http_resp.bytes()?.to_vec(),
                                        Err(e) => anyhow::bail!(e),
                                    }
                                } else if let Ok(mut f) = File::open(url) {
                                    // Read from local file
                                    let metadata = fs::metadata(url)?;
                                    #[allow(clippy::cast_possible_truncation)]
                                    let mut buffer = vec![0; metadata.len() as usize];
                                    f.read_exact(&mut buffer)?;
                                    buffer
                                } else {
                                    // Decode with base64
                                    general_purpose::STANDARD.decode(url)?
                                };
                                Ok(image::load_from_memory(&bytes)?)
                            })
                            .collect::<anyhow::Result<Vec<_>>>()
                    });
                    let images = match images {
                        Some(Ok(x)) => Some(x),
                        Some(Err(e)) => {
                            return anyhow::Result::Err(candle_core::Error::Msg(e.to_string()))
                        }
                        None => None,
                    };
                    seqs.push(new_dummy_seq(
                        tokens,
                        dummy_sender.clone(),
                        dummy_sampler.clone(),
                        dummy_group.clone(),
                        images,
                        (*self.get_metadata().tok_trie).clone(),
                    ));
                }
                let mut input_seqs = seqs.iter_mut().collect::<Vec<_>>();
                let inputs = inputs_processor
                    .process_inputs(
                        tokenizer.clone(),
                        &mut input_seqs,
                        true, // Always a prompt
                        metadata.is_xlora,
                        &device,
                        metadata.has_no_kv_cache,
                        None,
                        input_processor_cfg.clone(),
                        None, // TODO: get block tables/handle it for PagedAttention
                        None, // TODO: prompt chunking doesn't work.
                    )
                    .nth(0)
                    .unwrap();

                // === PREPARE AND RUN MODEL ==

                // Run the model, ignoring the logits
                let _ = target.forward_inputs(inputs.unwrap().inputs)?;

                // Clear the KV cache
                target.set_none_cache(true, true);

                // === BACKWARD STEP ==
                #[allow(clippy::cast_possible_truncation)]
                let labels = Tensor::from_vec(
                    batch
                        .iter()
                        .map(
                            |AnyMoeTrainingInputRow {
                                 prompt: _,
                                 expert,
                                 image_urls: _,
                             }| *expert as u32,
                        )
                        .collect::<Vec<_>>(),
                    (batch.len(),),
                    &device,
                )?;

                let cached = target.amoe_take_cached_gating_outputs();
                for (layer, (optimizer, output)) in optimizers.iter_mut().zip(cached).enumerate() {
                    let loss = candle_nn::loss::cross_entropy(
                        &output,
                        &labels.to_device(output.device())?,
                    )?;
                    let gradstore = loss.backward()?;
                    optimizer.step(&gradstore)?;
                    latest_loss[layer] = loss.to_dtype(DType::F32)?.to_scalar::<f32>()?;
                }
                all_losses.push(latest_loss.clone());
            }
        }

        target.amoe_finish_training(gate_model_id)?;
        assert_eq!(target.amoe_base_model_trainable_params(), 0);

        #[cfg(feature = "plotly")]
        if let Some(loss_svg) = loss_svg {
            let mut plot = Plot::new();
            for gate in 0..all_losses[0].len() {
                let gate_loss = all_losses
                    .iter()
                    .map(|losses| losses[gate])
                    .collect::<Vec<_>>();
                #[allow(clippy::cast_precision_loss)]
                plot.add_trace(Scatter::new(
                    (0..gate_loss.len())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|x| x as f32)
                        .collect::<Vec<_>>(),
                    gate_loss,
                ));
            }

            plot.set_layout(
                plot.layout()
                    .clone()
                    .show_legend(false)
                    .title(format!("Gating layers ({} layers)", all_losses[0].len()))
                    .x_axis(Axis::new().title("Step"))
                    .y_axis(Axis::new().title("Loss")),
            );

            let path = Path::new(&loss_svg);
            if !path
                .extension()
                .is_some_and(|e| e.to_string_lossy() == *"svg")
            {
                candle_core::bail!("`loss_svg` must have an extension `svg`.");
            }
            plot.write_image(path, ImageFormat::SVG, 800, 600, 1.0);
        }
        #[cfg(not(feature = "plotly"))]
        if let Some(_loss_svg) = loss_svg {}

        Ok(Some(AnyMoeTrainingResult {
            steps,
            final_loss: latest_loss,
        }))
    }
}

/// Create a dummy sequence containing just the prompt. This is OK because we just want a sequence that
/// has no information other than the input tokens (and maybe images).
fn new_dummy_seq(
    tokens: Vec<u32>,
    dummy_sender: tokio::sync::mpsc::Sender<Response>,
    dummy_sampler: Sampler,
    dummy_group: Arc<tokio::sync::Mutex<SequenceGroup>>,
    images: Option<Vec<DynamicImage>>,
    trie: TokTrie,
) -> Sequence {
    Sequence::new_waiting(
        tokens,
        0,
        0,
        1,
        dummy_sender,
        dummy_sampler,
        vec![],
        vec![],
        None,
        false,
        false,
        dummy_group,
        0,
        0,
        SequenceRecognizer::None,
        None,
        None,
        None,
        images,
        None, // TODO incorrect for PagedAttention
        trie,
        None,
    )
}
