#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use crate::{
    amoe::AnyMoeBaseModelMixin,
    layers::{Llama3RotaryEmbedding, ScaledDotProductAttention},
    lora::{linear_no_bias as linear, LinearLayerLike, LoraConfig, Ordering},
    paged_attention::ModelConfigMetadata,
    pipeline::{text_models_inputs_processor::PagedAttentionInputMetadata, IsqModel},
    utils::progress::NiceProgressBar,
};
use candle_core::{DType, Device, Result, Tensor};
use candle_nn::{embedding, Embedding, Module, VarBuilder};
use mistralrs_quant::QuantMethod;
use std::{collections::HashMap, sync::Arc};
use tqdm::Iter;
use tracing::info;

use crate::{
    device_map::DeviceMapper,
    layers::{repeat_kv, CausalMasker, RmsNorm},
    models::llama::Config,
    pipeline::{self, extract_logits, LayerCaches, NormalLoadingMetadata, NormalModel},
};

use super::{classifier::XLoraClassifier, NonGranularState, ScalingsMaker, XLoraConfig};

#[derive(Clone)]
struct CausalSelfAttention {
    q_proj: Arc<dyn LinearLayerLike + Send + Sync>,
    k_proj: Arc<dyn LinearLayerLike + Send + Sync>,
    v_proj: Arc<dyn LinearLayerLike + Send + Sync>,
    o_proj: Arc<dyn LinearLayerLike + Send + Sync>,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    head_dim: usize,
    use_flash_attn: bool,
    rotary_emb: Arc<Llama3RotaryEmbedding>,
    max_seq_len: usize,
}

impl CausalSelfAttention {
    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        x: &Tensor,
        mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut LayerCaches,
        scalings: Option<Tensor>,
        global_scaling_weight: f64,
        is_scaling_pass: Option<f64>,
    ) -> Result<Tensor> {
        let (b_sz, seq_len, hidden_size) = x.dims3()?;

        let original_dtype = x.dtype();
        let mut x = x.clone();
        if let Some(t) = self.q_proj.quantized_act_type() {
            x = x.to_dtype(t)?;
        }
        let mut q = self.q_proj.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?;
        let mut k = self.k_proj.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?;
        let mut v = self.v_proj.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?;
        if self.q_proj.quantized_act_type().is_some() {
            q = q.to_dtype(original_dtype)?;
            k = k.to_dtype(original_dtype)?;
            v = v.to_dtype(original_dtype)?;
        }

        let mut q = q.reshape((b_sz * seq_len, self.num_attention_heads, self.head_dim))?;
        let mut k = k.reshape((b_sz * seq_len, self.num_key_value_heads, self.head_dim))?;
        let v = if seq_len != 1 {
            v.reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
                .transpose(1, 2)?
        } else {
            // Optimization for seqlen = 1, avoid transpose and just modify reshape dims
            v.reshape((b_sz, self.num_key_value_heads, seq_len, self.head_dim))?
        };

        self.rotary_emb
            .forward(seqlen_offsets, &start_offsets_kernel, &mut q, &mut k, b_sz)?;

        if q.rank() == 3 && seq_len != 1 {
            q = q
                .reshape((b_sz, seq_len, self.num_attention_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
            k = k
                .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
                .transpose(1, 2)?
                .contiguous()?;
        } else if q.rank() == 3 {
            // Optimization for seqlen = 1, avoid transpose and just modify reshape dims
            q = q
                .reshape((b_sz, self.num_attention_heads, seq_len, self.head_dim))?
                .contiguous()?;
            k = k
                .reshape((b_sz, self.num_key_value_heads, seq_len, self.head_dim))?
                .contiguous()?;
        }

        let (k, v) =
            crate::pipeline::Cache::update_kv_cache(&mut kv_cache[block_idx], k, v, false)?;

        let k = repeat_kv(k, self.num_attention_heads / self.num_key_value_heads)?.contiguous()?;
        let v = repeat_kv(v, self.num_attention_heads / self.num_key_value_heads)?.contiguous()?;

        let y = ScaledDotProductAttention.run_attention(
            &q,
            &k,
            &v,
            self.num_attention_heads,
            self.head_dim,
            mask.clone().as_ref(),
            self.use_flash_attn,
            b_sz,
            seq_len,
        )?;

        let mut y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, hidden_size])?;
        if let Some(t) = self.q_proj.quantized_act_type() {
            y = y.to_dtype(t)?;
        }
        let mut res = self.o_proj.lora_forward(
            &y.transpose(1, 2)?.reshape((b_sz, seq_len, ()))?,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?;
        if self.q_proj.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    fn load(
        vb: VarBuilder,
        cfg: &Config,
        lora_config: &[((String, String), LoraConfig)],
        count: &mut usize,
        ord: &Ordering,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        rope: Arc<Llama3RotaryEmbedding>,
        preload_adapters: &Option<HashMap<String, (VarBuilder, LoraConfig)>>,
    ) -> Result<Self> {
        let size_in = cfg.hidden_size;
        let size_q = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_attention_heads;
        let size_kv = (cfg.hidden_size / cfg.num_attention_heads) * cfg.num_key_value_heads;
        let q_proj = linear(
            size_in,
            size_q,
            mapper.set_device(layer_idx, vb.pp("q_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("q_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        let k_proj = linear(
            size_in,
            size_kv,
            mapper.set_device(layer_idx, vb.pp("k_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("k_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        let v_proj = linear(
            size_in,
            size_kv,
            mapper.set_device(layer_idx, vb.pp("v_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("v_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        let o_proj = linear(
            size_q,
            size_in,
            mapper.set_device(layer_idx, vb.pp("o_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("o_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        Ok(Self {
            q_proj,
            k_proj,
            v_proj,
            o_proj,
            num_attention_heads: cfg.num_attention_heads,
            num_key_value_heads: cfg.num_key_value_heads,
            head_dim: cfg.hidden_size / cfg.num_attention_heads,
            use_flash_attn: cfg.use_flash_attn,
            rotary_emb: rope,
            max_seq_len: cfg.max_position_embeddings,
        })
    }
}

#[derive(Clone)]
struct Mlp {
    c_fc1: Arc<dyn LinearLayerLike + Send + Sync>,
    c_fc2: Arc<dyn LinearLayerLike + Send + Sync>,
    c_proj: Arc<dyn LinearLayerLike + Send + Sync>,
}

impl Mlp {
    fn forward(
        &self,
        x: &Tensor,
        scalings: Option<Tensor>,
        global_scaling_weight: f64,
        is_scaling_pass: Option<f64>,
    ) -> Result<Tensor> {
        let original_dtype = x.dtype();
        let mut x = x.clone();
        if let Some(t) = self.c_fc1.quantized_act_type() {
            x = x.to_dtype(t)?;
        }
        let x = (candle_nn::ops::silu(&self.c_fc1.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?)? * self.c_fc2.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?)?;
        let mut res = self.c_proj.lora_forward(
            &x,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )?;
        if self.c_fc1.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    fn load(
        vb: VarBuilder,
        cfg: &Config,
        lora_config: &[((String, String), LoraConfig)],
        count: &mut usize,
        ord: &Ordering,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        preload_adapters: &Option<HashMap<String, (VarBuilder, LoraConfig)>>,
    ) -> Result<Self> {
        let h_size = cfg.hidden_size;
        let i_size = cfg.intermediate_size;
        let c_fc1 = linear(
            h_size,
            i_size,
            mapper.set_device(layer_idx, vb.pp("gate_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("gate_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        let c_fc2 = linear(
            h_size,
            i_size,
            mapper.set_device(layer_idx, vb.pp("up_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("up_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        let c_proj = linear(
            i_size,
            h_size,
            mapper.set_device(layer_idx, vb.pp("down_proj"), loading_isq),
            mapper.set_device(layer_idx, vb.pp("down_proj"), false),
            lora_config,
            count,
            ord,
            preload_adapters,
        )?;
        Ok(Self {
            c_fc1,
            c_fc2,
            c_proj,
        })
    }
}

#[derive(Clone)]
struct Block {
    rms_1: RmsNorm,
    attn: CausalSelfAttention,
    rms_2: RmsNorm,
    mlp: Mlp,
}

impl Block {
    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        x: &Tensor,
        mask: &Option<Tensor>,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        block_idx: usize,
        kv_cache: &mut LayerCaches,
        scalings: Option<Tensor>,
        global_scaling_weight: f64,
        is_scaling_pass: Option<f64>,
    ) -> Result<Tensor> {
        let residual = x;
        let x = self.rms_1.forward(x)?;
        let x = (self.attn.forward(
            &x,
            mask,
            seqlen_offsets,
            start_offsets_kernel,
            block_idx,
            kv_cache,
            scalings.clone(),
            global_scaling_weight,
            is_scaling_pass,
        )? + residual)?;
        let residual = &x;
        let x = (self.mlp.forward(
            &self.rms_2.forward(&x)?,
            scalings,
            global_scaling_weight,
            is_scaling_pass,
        )? + residual)?;
        Ok(x)
    }

    #[allow(clippy::too_many_arguments)]
    fn load(
        vb: VarBuilder,
        cfg: &Config,
        lora_config: &[((String, String), LoraConfig)],
        count: &mut usize,
        ord: &Ordering,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        rope: Arc<Llama3RotaryEmbedding>,
        preload_adapters: &Option<HashMap<String, (VarBuilder, LoraConfig)>>,
    ) -> Result<Self> {
        let attn = CausalSelfAttention::load(
            vb.pp("self_attn"),
            cfg,
            lora_config,
            count,
            ord,
            mapper,
            layer_idx,
            loading_isq,
            rope,
            preload_adapters,
        )?;
        let mlp = Mlp::load(
            vb.pp("mlp"),
            cfg,
            lora_config,
            count,
            ord,
            mapper,
            layer_idx,
            loading_isq,
            preload_adapters,
        )?;
        let rms_1 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_device(layer_idx, vb.pp("input_layernorm"), false),
        )?;
        let rms_2 = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_device(layer_idx, vb.pp("post_attention_layernorm"), false),
        )?;
        Ok(Self {
            rms_1,
            attn,
            rms_2,
            mlp,
        })
    }
}

pub struct XLoraLlama {
    wte: Embedding,
    blocks: Vec<Block>,
    ln_f: RmsNorm,
    lm_head: Arc<dyn LinearLayerLike + Send + Sync>,
    pub kv_cache: pipeline::Cache,
    pub device: Device,
    xlora_classifier: Option<XLoraClassifier>,
    dtype: DType,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
    cfg: ModelConfigMetadata,
}

impl XLoraLlama {
    #[allow(clippy::too_many_arguments)]
    fn inner_forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        scalings: Option<Tensor>,
        is_full_pass: bool,
        no_kv_cache: bool,
        is_scaling_pass: Option<f64>,
    ) -> Result<Tensor> {
        let mut x = self.wte.forward(input_ids)?;
        let mut cache = if is_full_pass {
            if no_kv_cache {
                let mut new_cache = Vec::new();
                for _ in 0..self.kv_cache.xlora_lock().len() {
                    new_cache.push(None);
                }

                self.kv_cache.xlora_lock().clone_from(&new_cache);
            }
            self.kv_cache.xlora_lock()
        } else {
            self.kv_cache.lock()
        };
        let mask = CausalMasker.make_causal_mask_as_attn_bias(
            input_ids,
            &*cache,
            x.dtype(),
            self.blocks[0].attn.num_attention_heads,
        )?;
        for (block_idx, block) in self.blocks.iter().enumerate() {
            x = self.mapper.map(x, block_idx)?;
            x = block.forward(
                &x,
                &mask.clone().map(|m| m.to_device(x.device()).unwrap()),
                seqlen_offsets,
                start_offsets_kernel.clone(),
                block_idx,
                &mut cache,
                scalings.clone(),
                self.xlora_classifier
                    .as_ref()
                    .map(|classifier| classifier.get_global_scaling_weight())
                    .unwrap_or(1.0),
                is_scaling_pass,
            )?;
        }
        let x = x.to_device(&self.device)?;
        self.ln_f.forward(&x)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn forward(
        &self,
        input_ids: &Tensor,
        input_ids_full: &Tensor,
        seqlen_offsets: &[usize],
        seqlen_offsets_full: &[usize],
        start_offsets_kernel: Tensor,
        start_offsets_kernel_full: Tensor,
        no_kv_cache: bool,
        non_granular_state: &Option<NonGranularState>,
        context_lens: Vec<(usize, usize)>,
    ) -> Result<Tensor> {
        if self.xlora_classifier.is_some() {
            let scalings = self.get_scalings(
                input_ids,
                input_ids_full,
                seqlen_offsets,
                seqlen_offsets_full,
                &start_offsets_kernel,
                &start_offsets_kernel_full,
                no_kv_cache,
                non_granular_state,
                &vec![usize::MAX; context_lens.len()],
            )?;

            if no_kv_cache {
                let mut res = self
                    .inner_forward(
                        input_ids_full,
                        seqlen_offsets_full,
                        start_offsets_kernel_full,
                        Some(scalings),
                        true,
                        no_kv_cache,
                        None,
                    )?
                    .contiguous()?;
                if let Some(t) = self.lm_head.quantized_act_type() {
                    res = res.to_dtype(t)?;
                }
                extract_logits(
                    &self.lm_head.lora_forward(&res, None, 1.0, None)?,
                    context_lens,
                )
            } else {
                // is_full_pass=true is ok because no_kv_cache=false
                let mut res = self
                    .inner_forward(
                        input_ids,
                        seqlen_offsets,
                        start_offsets_kernel,
                        Some(scalings),
                        true,
                        no_kv_cache,
                        None,
                    )?
                    .contiguous()?;
                if let Some(t) = self.lm_head.quantized_act_type() {
                    res = res.to_dtype(t)?;
                }
                extract_logits(
                    &self.lm_head.lora_forward(&res, None, 1.0, None)?,
                    context_lens,
                )
            }
        } else {
            let mut res = self
                .inner_forward(
                    input_ids,
                    seqlen_offsets,
                    start_offsets_kernel,
                    None,
                    false,
                    no_kv_cache,
                    None,
                )?
                .contiguous()?;
            if let Some(t) = self.lm_head.quantized_act_type() {
                res = res.to_dtype(t)?;
            }
            extract_logits(
                &self.lm_head.lora_forward(&res, None, 1.0, None)?,
                context_lens,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &Config,
        vb: VarBuilder,
        lora_config: &[((String, String), LoraConfig)],
        xlora_config: Option<XLoraConfig>,
        xlora_ordering: Ordering,
        is_gptx: bool,
        normal_loading_metadata: NormalLoadingMetadata,
        preload_adapters: &Option<HashMap<String, (VarBuilder, LoraConfig)>>,
    ) -> Result<Self> {
        if let Some(ref quant_cfg) = &cfg.quantization_config {
            tracing::info!(
                "Using {} quantization in {} bits.",
                quant_cfg.quant_method.to_string(),
                quant_cfg.bits
            );
        }
        let mapper = normal_loading_metadata.mapper;
        let dtype = vb.dtype();
        let mut count = 0;

        let wte = embedding(
            cfg.vocab_size,
            cfg.hidden_size,
            mapper.set_nm_device(vb.pp("model.embed_tokens"), false),
        )?;
        let lm_head = linear(
            cfg.hidden_size,
            cfg.vocab_size,
            mapper.set_nm_device(vb.pp("lm_head"), normal_loading_metadata.loading_isq),
            mapper.set_nm_device(vb.pp("lm_head"), false),
            lora_config,
            &mut count,
            &xlora_ordering,
            preload_adapters,
        )?;
        if xlora_config.is_some() && lm_head.is_lora() {
            // This is why we can pass dummy values (..., None, 1.0, None)?
            candle_core::bail!("Got an adapter `lm_head` layer, this is unsupported with X-LoRA.");
        }
        let ln_f = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            mapper.set_nm_device(vb.pp("model.norm"), false),
        )?;
        let mut blocks: Vec<_> =
            NiceProgressBar::<_, 'b'>(0..cfg.num_hidden_layers, "Loading repeating layers")
                .into_iter()
                .map(|i| {
                    let rotary_emb = Arc::new(
                        Llama3RotaryEmbedding::new(
                            vb.dtype(),
                            cfg,
                            mapper
                                .device_for(i, false)
                                .unwrap_or(&normal_loading_metadata.real_device),
                            is_gptx,
                        )
                        .expect("Failed to create RoPE"),
                    );
                    Block::load(
                        vb.pp(&format!("model.layers.{i}")),
                        cfg,
                        lora_config,
                        &mut count,
                        &xlora_ordering,
                        &*mapper,
                        i,
                        normal_loading_metadata.loading_isq,
                        rotary_emb,
                        preload_adapters,
                    )
                    .expect("Failed to load block.")
                })
                .collect();
        if xlora_config.is_none() && preload_adapters.is_none() {
            // We are now a LoRA model so we must merge the weights
            info!("Merging LoRA adapters.");
            for layer in blocks.iter_mut().tqdm() {
                Arc::get_mut(&mut layer.attn.k_proj)
                    .unwrap()
                    .merge_weights()?;
                Arc::get_mut(&mut layer.attn.o_proj)
                    .unwrap()
                    .merge_weights()?;
                Arc::get_mut(&mut layer.attn.q_proj)
                    .unwrap()
                    .merge_weights()?;
                Arc::get_mut(&mut layer.attn.v_proj)
                    .unwrap()
                    .merge_weights()?;

                Arc::get_mut(&mut layer.mlp.c_fc1)
                    .unwrap()
                    .merge_weights()?;
                Arc::get_mut(&mut layer.mlp.c_fc2)
                    .unwrap()
                    .merge_weights()?;
                Arc::get_mut(&mut layer.mlp.c_proj)
                    .unwrap()
                    .merge_weights()?;
            }
        }

        Ok(Self {
            wte,
            blocks,
            ln_f,
            lm_head,
            kv_cache: pipeline::Cache::new(cfg.num_hidden_layers, true),
            device: normal_loading_metadata.real_device,
            xlora_classifier: xlora_config.map(|xlora_config| {
                XLoraClassifier::new(xlora_config, count, lora_config.len(), vb, false).unwrap()
            }),
            dtype,
            mapper,
            cfg: ModelConfigMetadata {
                num_layers: cfg.num_hidden_layers,
                hidden_size: cfg.hidden_size,
                num_kv_heads: cfg.num_key_value_heads,
                num_attn_heads: cfg.num_attention_heads,
                sliding_window: None,
                head_dim: None,
            },
        })
    }
}

impl IsqModel for XLoraLlama {
    fn get_layers(
        &mut self,
    ) -> (
        Vec<(&mut Arc<dyn QuantMethod>, Option<usize>)>,
        &dyn DeviceMapper,
    ) {
        let mut tensors = Vec::new();
        tensors.push((Arc::get_mut(&mut self.lm_head).unwrap().quant_inner(), None));
        for (i, layer) in self.blocks.iter_mut().enumerate() {
            tensors.push((
                Arc::get_mut(&mut layer.attn.q_proj).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.attn.k_proj).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.attn.v_proj).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.attn.o_proj).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.mlp.c_fc1).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.mlp.c_fc2).unwrap().quant_inner(),
                Some(i),
            ));
            tensors.push((
                Arc::get_mut(&mut layer.mlp.c_proj).unwrap().quant_inner(),
                Some(i),
            ));
        }
        (tensors, &*self.mapper)
    }
}

impl NormalModel for XLoraLlama {
    fn forward(
        &self,
        _input_ids: &Tensor,
        _seqlen_offsets: &[usize],
        _start_offsets_kernel: Tensor,
        _context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
        _metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
    ) -> Result<Tensor> {
        unreachable!()
    }
    fn xlora_forward(
        &self,
        input_ids: &Tensor,
        input_ids_full: &Tensor,
        seqlen_offsets: &[usize],
        seqlen_offsets_full: &[usize],
        start_offsets_kernel: Tensor,
        start_offsets_kernel_full: Tensor,
        no_kv_cache: bool,
        non_granular_state: &Option<crate::xlora_models::NonGranularState>,
        context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
    ) -> Result<Tensor> {
        self.forward(
            input_ids,
            input_ids_full,
            seqlen_offsets,
            seqlen_offsets_full,
            start_offsets_kernel,
            start_offsets_kernel_full,
            no_kv_cache,
            non_granular_state,
            context_lens,
        )
    }
    fn cache(&self) -> &super::Cache {
        &self.kv_cache
    }
    fn device(&self) -> &Device {
        &self.device
    }
    fn is_xlora(&self) -> bool {
        true
    }
    fn max_seq_len(&self) -> usize {
        self.blocks[0].attn.max_seq_len
    }
    fn activate_adapters(&mut self, adapter_names: Vec<String>) -> Result<usize> {
        if self.xlora_classifier.is_some() {
            candle_core::bail!("Adapter activation is not supported for X-LoRA models as the adapter set must remain the same.");
        }
        let mut sum = 0;
        for layer in self.blocks.iter_mut() {
            sum += Arc::get_mut(&mut layer.attn.k_proj)
                .unwrap()
                .activate(&adapter_names)?;
            sum += Arc::get_mut(&mut layer.attn.o_proj)
                .unwrap()
                .activate(&adapter_names)?;
            sum += Arc::get_mut(&mut layer.attn.q_proj)
                .unwrap()
                .activate(&adapter_names)?;
            sum += Arc::get_mut(&mut layer.attn.v_proj)
                .unwrap()
                .activate(&adapter_names)?;

            sum += Arc::get_mut(&mut layer.mlp.c_fc1)
                .unwrap()
                .activate(&adapter_names)?;
            sum += Arc::get_mut(&mut layer.mlp.c_fc2)
                .unwrap()
                .activate(&adapter_names)?;
            sum += Arc::get_mut(&mut layer.mlp.c_proj)
                .unwrap()
                .activate(&adapter_names)?;
        }
        Ok(sum)
    }
    fn config(&self) -> &ModelConfigMetadata {
        &self.cfg
    }
}

impl ScalingsMaker for XLoraLlama {
    fn dtype(&self) -> DType {
        self.dtype
    }
    fn get_cache(&self) -> &pipeline::Cache {
        &self.kv_cache
    }
    fn get_classifier(&self) -> &XLoraClassifier {
        self.xlora_classifier.as_ref().unwrap()
    }
    fn forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        start_offsets_kernel: Tensor,
        scalings: Tensor,
        is_full_pass: bool,
        no_kv_cache: bool,
        is_scaling_pass: Option<f64>,
        _context_lens: &[usize],
    ) -> Result<Tensor> {
        self.inner_forward(
            input_ids,
            seqlen_offsets,
            start_offsets_kernel,
            Some(scalings),
            is_full_pass,
            no_kv_cache,
            is_scaling_pass,
        )
    }
}

impl AnyMoeBaseModelMixin for XLoraLlama {}
