#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

// This implementation is based on:
// https://huggingface.co/microsoft/Phi-3-mini-4k-instruct/blob/main/modeling_phi3.py
use candle_core::{Device, IndexOp, Module, Result, Tensor, D};
use candle_nn::{layer_norm, LayerNorm, LayerNormConfig, VarBuilder};
use mistralrs_quant::{QuantMethod, QuantMethodConfig, QuantizedConfig, UnquantLinear};
use std::sync::Arc;

use crate::{
    amoe::AnyMoeBaseModelMixin,
    attention::SdpaParams,
    device_map::DeviceMapper,
    layers::{CausalMasker, MatMul, PhiRopeConfig, PhiRopeScalingConfig, PhiRotaryEmbedding, Sdpa},
    layers_masker::{masked_fill, PastKvLenCache},
    ops::NonZeroOp,
    paged_attention::{AttentionImplementation, ModelConfigMetadata, PagedAttention},
    pipeline::{
        extract_logits,
        text_models_inputs_processor::{FlashParams, PagedAttentionInputMetadata},
        Cache, IsqModel, NormalLoadingMetadata, NormalModel,
    },
    utils::progress::NiceProgressBar,
};

// https://huggingface.co/microsoft/Phi-3-mini-4k-instruct/blob/main/config.json
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct Config {
    pub(crate) vocab_size: usize,
    pub(crate) hidden_act: candle_nn::Activation,
    pub(crate) hidden_size: usize,
    pub(crate) intermediate_size: usize,
    pub(crate) num_hidden_layers: usize,
    pub(crate) num_attention_heads: usize,
    pub(crate) num_key_value_heads: usize,
    pub(crate) rms_norm_eps: f64,
    pub(crate) rope_theta: f64,
    pub(crate) rope_scaling: Option<PhiRopeScalingConfig>,
    pub(crate) max_position_embeddings: usize,
    pub(crate) use_flash_attn: bool,
    pub(crate) sliding_window: Option<usize>,
    pub(crate) original_max_position_embeddings: usize,
    pub(crate) quantization_config: Option<QuantizedConfig>,
    pub(crate) lm_head_bias: bool,
    pub(crate) attention_bias: bool,
    pub(crate) num_local_experts: usize,
    pub(crate) router_jitter_noise: f64,
}

impl From<Config> for PhiRopeConfig {
    fn from(val: Config) -> Self {
        PhiRopeConfig {
            rope_scaling: val.rope_scaling,
            max_position_embeddings: val.max_position_embeddings,
            original_max_position_embeddings: val.original_max_position_embeddings,
            rope_theta: val.rope_theta,
            head_dim: val.hidden_size / val.num_attention_heads,
        }
    }
}

impl Config {
    pub fn head_dim(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

struct Attention {
    q_proj: Arc<dyn QuantMethod>,
    k_proj: Arc<dyn QuantMethod>,
    v_proj: Arc<dyn QuantMethod>,
    o_proj: Arc<dyn QuantMethod>,
    num_heads: usize,
    num_kv_heads: usize,
    head_dim: usize,
    rotary_emb: Arc<PhiRotaryEmbedding>,
    sliding_window: Option<usize>,
    paged_attn: Option<PagedAttention>,
    sdpa_params: SdpaParams,
}

impl Attention {
    fn new(
        rotary_emb: Arc<PhiRotaryEmbedding>,
        cfg: &Config,
        vb: VarBuilder,
        paged_attn: Option<PagedAttention>,
    ) -> Result<Self> {
        let num_heads = cfg.num_attention_heads;
        let num_kv_heads = cfg.num_key_value_heads;
        let head_dim = cfg.head_dim();

        let q_proj = mistralrs_quant::linear_b(
            cfg.hidden_size,
            num_heads * head_dim,
            cfg.attention_bias,
            &cfg.quantization_config,
            vb.pp("q_proj"),
        )?;
        let k_proj = mistralrs_quant::linear_b(
            cfg.hidden_size,
            num_kv_heads * head_dim,
            cfg.attention_bias,
            &cfg.quantization_config,
            vb.pp("k_proj"),
        )?;
        let v_proj = mistralrs_quant::linear_b(
            cfg.hidden_size,
            num_kv_heads * head_dim,
            cfg.attention_bias,
            &cfg.quantization_config,
            vb.pp("v_proj"),
        )?;
        let o_proj = mistralrs_quant::linear_b(
            num_heads * head_dim,
            cfg.hidden_size,
            cfg.attention_bias,
            &cfg.quantization_config,
            vb.pp("o_proj"),
        )?;

        Ok(Self {
            q_proj,
            k_proj,
            v_proj,
            o_proj,
            rotary_emb,
            num_heads,
            num_kv_heads,
            head_dim,
            sliding_window: cfg.sliding_window,
            paged_attn,
            sdpa_params: SdpaParams {
                n_kv_groups: num_heads / num_kv_heads,
                use_flash_attn: cfg.use_flash_attn,
                softcap: None,
                softmax_scale: 1.0 / (head_dim as f32).sqrt(),
                sliding_window: cfg.sliding_window,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        xs: &Tensor,
        attention_mask: Option<&Tensor>,
        seqlen_offsets: &[usize],
        position_ids: &[usize],
        kv_cache: &mut Option<(Tensor, Tensor)>,
        metadata: Option<((Tensor, Tensor), &mut PagedAttentionInputMetadata)>,
        flash_params: &FlashParams,
    ) -> Result<Tensor> {
        let (b_sz, q_len, _) = xs.dims3()?;

        let original_dtype = xs.dtype();
        let mut xs = xs.clone();
        if let Some(t) = self.q_proj.quantized_act_type() {
            xs = xs.to_dtype(t)?;
        }
        let mut q = MatMul.qmethod_matmul(&xs, &*self.q_proj)?;
        let mut k = MatMul.qmethod_matmul(&xs, &*self.k_proj)?;
        let mut v = MatMul.qmethod_matmul(&xs, &*self.v_proj)?;
        if self.q_proj.quantized_act_type().is_some() {
            q = q.to_dtype(original_dtype)?;
            k = k.to_dtype(original_dtype)?;
            v = v.to_dtype(original_dtype)?;
        }

        let (q, k, v) = if q_len != 1 {
            let q = q
                .reshape((b_sz, q_len, self.num_heads, self.head_dim))?
                .transpose(1, 2)?;
            let k = k
                .reshape((b_sz, q_len, self.num_kv_heads, self.head_dim))?
                .transpose(1, 2)?;
            let v = v
                .reshape((b_sz, q_len, self.num_kv_heads, self.head_dim))?
                .transpose(1, 2)?;
            (q, k, v)
        } else {
            let q = q.reshape((b_sz, self.num_heads, q_len, self.head_dim))?;
            let k = k.reshape((b_sz, self.num_kv_heads, q_len, self.head_dim))?;
            let v = v.reshape((b_sz, self.num_kv_heads, q_len, self.head_dim))?;
            (q, k, v)
        };

        let (q, k) = self
            .rotary_emb
            .forward(&q, &k, seqlen_offsets, position_ids)?;

        let mut attn_output = match &self.paged_attn {
            Some(paged_attn) => {
                let ((key_cache, value_cache), input_metadata) = metadata.unwrap();
                paged_attn.forward(
                    &q,
                    &k,
                    &v,
                    attention_mask,
                    Some(key_cache),
                    Some(value_cache),
                    input_metadata,
                    None,
                )?
            }
            _ => {
                let (k, v, attn_mask) = Cache::update_kv_cache_sliding_window(
                    kv_cache,
                    k,
                    v,
                    attention_mask,
                    self.sliding_window,
                    true,
                )?;

                Sdpa.run_attention(
                    &q,
                    &k,
                    &v,
                    attn_mask.as_ref(),
                    Some(flash_params),
                    &self.sdpa_params,
                )?
            }
        };

        if let Some(t) = self.q_proj.quantized_act_type() {
            attn_output = attn_output.to_dtype(t)?;
        }
        attn_output = if attention_mask.is_some() {
            attn_output.transpose(1, 2)?.reshape((b_sz, q_len, ()))?
        } else {
            attn_output.reshape((b_sz, q_len, ()))?
        };
        let mut res = MatMul.qmethod_matmul(&attn_output, &*self.o_proj)?;
        if self.q_proj.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }
}

#[derive(Clone)]
struct Mlp {
    w1: Arc<dyn QuantMethod>,
    w2: Arc<dyn QuantMethod>,
    w3: Arc<dyn QuantMethod>,
    act_fn: candle_nn::Activation,
}

impl Mlp {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let hidden_size = cfg.hidden_size;
        let i_size = cfg.intermediate_size;

        let w1 = mistralrs_quant::linear_no_bias(
            hidden_size,
            i_size,
            &cfg.quantization_config,
            vb.pp("w1"),
        )?;
        let w2 = mistralrs_quant::linear_no_bias(
            i_size,
            hidden_size,
            &cfg.quantization_config,
            vb.pp("w2"),
        )?;
        let w3 = mistralrs_quant::linear_no_bias(
            hidden_size,
            i_size,
            &cfg.quantization_config,
            vb.pp("w3"),
        )?;

        Ok(Self {
            w1,
            w2,
            w3,
            act_fn: cfg.hidden_act,
        })
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let original_dtype = xs.dtype();
        let mut xs = xs.clone();
        if let Some(t) = self.w1.quantized_act_type() {
            xs = xs.to_dtype(t)?;
        }
        let mut current_hidden_states =
            MatMul.qmethod_matmul(&xs, &*self.w1)?.apply(&self.act_fn)?;
        let rhs = MatMul.qmethod_matmul(&xs, &*self.w3)?;
        current_hidden_states = current_hidden_states.broadcast_mul(&rhs)?;
        let mut res = MatMul.qmethod_matmul(&current_hidden_states, &*self.w2)?;
        if self.w1.quantized_act_type().is_some() {
            res = res.to_dtype(original_dtype)?;
        }
        Ok(res)
    }
}

struct MoeMlp {
    gate: Arc<dyn QuantMethod>,
    experts: Vec<Mlp>,
    router_jitter_noise: f64,
    num_experts: usize,
}

impl MoeMlp {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let num_experts = cfg.num_local_experts;
        let gate = mistralrs_quant::linear_no_bias(
            cfg.hidden_size,
            num_experts,
            &cfg.quantization_config,
            vb.pp("gate"),
        )?;

        let experts_vb = vb.pp("experts");
        let mut experts = Vec::with_capacity(num_experts);
        for i in 0..num_experts {
            experts.push(Mlp::new(cfg, experts_vb.pp(i))?);
        }

        Ok(Self {
            gate,
            experts,
            router_jitter_noise: cfg.router_jitter_noise,
            num_experts,
        })
    }

    fn sparsemixer(&self, scores: &Tensor, jitter_eps: f64) -> Result<(Tensor, Tensor)> {
        // Compute mask for sparsity
        let selected_experts = scores.argmax_keepdim(D::Minus1)?;
        let mask_logits_threshold = scores.gather(&selected_experts, D::Minus1)?;
        let factor = scores.abs()?.broadcast_minimum(&mask_logits_threshold)?;
        let mask_logits_threshold = mask_logits_threshold
            .broadcast_sub(scores)?
            .broadcast_div(&factor)?
            .gt(2. * jitter_eps)?;

        // Apply mask
        let masked_gates = masked_fill(scores, &mask_logits_threshold, f64::NEG_INFINITY)?;

        // Compute scores
        let masked_gates = candle_nn::ops::softmax_last_dim(&masked_gates)?;
        let multiplier = masked_gates.gather(&selected_experts, D::Minus1)?;

        // Mask out first expert
        let masked_scores = scores.scatter_add(
            &selected_experts
                .broadcast_as(scores.shape())?
                .contiguous()?,
            &(scores.ones_like()? * f64::NEG_INFINITY)?,
            D::Minus1,
        )?;

        // Compute mask for sparsity
        let selected_experts_top2 = masked_scores.argmax_keepdim(D::Minus1)?;
        let mask_logits_threshold = masked_scores.gather(&selected_experts_top2, D::Minus1)?;
        let factor = scores.abs()?.broadcast_minimum(&mask_logits_threshold)?;
        let mask_logits_threshold = mask_logits_threshold
            .broadcast_sub(scores)?
            .broadcast_div(&factor)?
            .gt(2. * jitter_eps)?;

        // Apply mask
        let masked_gates_top2 =
            masked_fill(&masked_scores, &mask_logits_threshold, f64::NEG_INFINITY)?;
        let masked_gates_top2 = candle_nn::ops::softmax_last_dim(&masked_gates_top2)?;
        let multiplier_top2 = masked_gates_top2.gather(&selected_experts_top2, D::Minus1)?;

        let multiplier = Tensor::cat(&[multiplier, multiplier_top2], D::Minus1)?;
        let selected_experts = Tensor::cat(&[selected_experts, selected_experts_top2], D::Minus1)?;

        Ok((multiplier, selected_experts))
    }

    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let (bs, seq, hidden) = xs.dims3()?;
        let mut xs = xs.reshape(((), hidden))?;
        let original_dtype = xs.dtype();
        if let Some(t) = self.gate.quantized_act_type() {
            xs = xs.to_dtype(t)?;
        }
        let mut router_logits = MatMul.qmethod_matmul(&xs, &*self.gate)?;
        if self.gate.quantized_act_type().is_some() {
            router_logits = router_logits.to_dtype(original_dtype)?;
            xs = xs.to_dtype(original_dtype)?;
        }
        let (routing_weights, selected_experts) =
            self.sparsemixer(&router_logits, self.router_jitter_noise)?;

        let mut final_hidden_states = Tensor::zeros((bs * seq, hidden), xs.dtype(), xs.device())?;

        // One hot encode the selected experts to create an expert mask
        // this will be used to easily index which expert to activate
        let experts_mask =
            candle_nn::encoding::one_hot(selected_experts, self.num_experts, 1u8, 0u8)?
                .permute((2, 1, 0))?;

        // Loop over all avail experts in the model and perform the computation on each expert
        for expert_idx in 0..self.num_experts {
            let expert = &self.experts[expert_idx];
            let expert_mask = experts_mask.i(expert_idx)?;
            assert_eq!(expert_mask.rank(), 2);
            let nonzero_mask = expert_mask.contiguous()?.nonzero()?;
            let idx = nonzero_mask.i((.., 0))?;
            let top_x = nonzero_mask.i((.., 1))?;

            if top_x.dim(0)? == 0 {
                continue;
            }

            // Index the correct hidden staters and compute the expert hidden state
            // for the current expert, we need to make sure to multiply the output hidden
            // states by `routing_weights` on the corresponding tokens (top-1, top-2)
            let current_state = xs.index_select(&top_x, 0)?.reshape(((), hidden))?;
            let current_routing_weights = routing_weights
                .index_select(&top_x, 0)?
                .gather(&idx.unsqueeze(1)?.contiguous()?, 1)?;
            let exp_out = expert.forward(&current_state)?;

            let current_hidden_states = exp_out.broadcast_mul(&current_routing_weights)?;

            final_hidden_states = final_hidden_states.index_add(
                &top_x.contiguous()?,
                &current_hidden_states.to_dtype(xs.dtype())?,
                0,
            )?;
        }

        final_hidden_states.reshape((bs, seq, hidden))
    }
}

struct DecoderLayer {
    self_attn: Attention,
    mlp: MoeMlp,
    input_layernorm: LayerNorm,
    post_attention_layernorm: LayerNorm,
}

impl DecoderLayer {
    fn new(
        rotary_emb: Arc<PhiRotaryEmbedding>,
        cfg: &Config,
        vb: VarBuilder,
        mapper: &dyn DeviceMapper,
        layer_idx: usize,
        loading_isq: bool,
        paged_attn: Option<PagedAttention>,
    ) -> Result<Self> {
        let self_attn = Attention::new(
            rotary_emb,
            cfg,
            mapper.set_device(layer_idx, vb.pp("self_attn"), loading_isq),
            paged_attn,
        )?;
        let mlp = MoeMlp::new(
            cfg,
            mapper.set_device(layer_idx, vb.pp("block_sparse_moe"), loading_isq),
        )?;
        let input_layernorm = layer_norm(
            cfg.hidden_size,
            LayerNormConfig {
                eps: cfg.rms_norm_eps,
                remove_mean: true,
                affine: true,
            },
            mapper.set_device(layer_idx, vb.pp("input_layernorm"), false),
        )?;
        let post_attention_layernorm = layer_norm(
            cfg.hidden_size,
            LayerNormConfig {
                eps: cfg.rms_norm_eps,
                remove_mean: true,
                affine: true,
            },
            mapper.set_device(layer_idx, vb.pp("post_attention_layernorm"), false),
        )?;
        Ok(Self {
            self_attn,
            mlp,
            input_layernorm,
            post_attention_layernorm,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        xs: &Tensor,
        attention_mask: Option<&Tensor>,
        seqlen_offsets: &[usize],
        position_ids: &[usize],
        kv_cache: &mut Option<(Tensor, Tensor)>,
        metadata: Option<((Tensor, Tensor), &mut PagedAttentionInputMetadata)>,
        flash_params: &FlashParams,
    ) -> Result<Tensor> {
        let residual = xs;
        let xs = self.input_layernorm.forward(xs)?;
        let xs = self.self_attn.forward(
            &xs,
            attention_mask,
            seqlen_offsets,
            position_ids,
            kv_cache,
            metadata,
            flash_params,
        )?;
        let xs = (xs + residual)?;
        let residual = &xs;
        let xs = self
            .mlp
            .forward(&xs.apply(&self.post_attention_layernorm)?)?;
        residual + xs
    }
}

pub struct Model {
    embed_tokens: candle_nn::Embedding,
    layers: Vec<DecoderLayer>,
    norm: LayerNorm,
    lm_head: Arc<dyn QuantMethod>,
    device: Device,
    cache: Cache,
    max_seq_len: usize,
    mapper: Box<dyn DeviceMapper + Send + Sync>,
    sliding_window: Option<usize>,
    cfg: ModelConfigMetadata,
}

impl Model {
    pub fn new(
        cfg: &Config,
        vb: VarBuilder,
        _is_gptx: bool,
        normal_loading_metadata: NormalLoadingMetadata,
        attention_mechanism: AttentionImplementation,
    ) -> Result<Self> {
        if let Some(ref quant_cfg) = &cfg.quantization_config {
            tracing::info!(
                "Using {} quantization in {} bits.",
                quant_cfg.quant_method.to_string(),
                quant_cfg.bits
            );
        }
        let mapper = normal_loading_metadata.mapper;
        let vb_m = vb.pp("model");

        let embed_tokens = candle_nn::embedding(
            cfg.vocab_size,
            cfg.hidden_size,
            mapper.set_nm_device(vb_m.pp("embed_tokens"), false),
        )?;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        let vb_l = vb_m.pp("layers");
        for layer_idx in
            NiceProgressBar::<_, 'b'>(0..cfg.num_hidden_layers, "Loading repeating layers")
        {
            let device = mapper
                .device_for(layer_idx, false)
                .unwrap_or(&normal_loading_metadata.real_device);
            let rotary_emb = Arc::new(PhiRotaryEmbedding::new(vb.dtype(), cfg.clone(), device)?);
            let paged_attn = match &attention_mechanism {
                AttentionImplementation::Eager => None,
                AttentionImplementation::PagedAttention => Some(PagedAttention::new(
                    cfg.num_attention_heads,
                    cfg.head_dim(),
                    (1.0 / (cfg.head_dim() as f64).sqrt()) as f32,
                    Some(cfg.num_key_value_heads),
                    cfg.sliding_window,
                    device,
                    None,
                )?),
            };
            let layer = DecoderLayer::new(
                rotary_emb.clone(),
                cfg,
                vb_l.pp(layer_idx),
                &*mapper,
                layer_idx,
                normal_loading_metadata.loading_isq,
                paged_attn,
            )?;
            layers.push(layer)
        }
        let norm = layer_norm(
            cfg.hidden_size,
            LayerNormConfig {
                eps: cfg.rms_norm_eps,
                remove_mean: true,
                affine: true,
            },
            mapper.set_nm_device(vb_m.pp("norm"), false),
        )?;
        let lm_head = candle_nn::linear_b(
            cfg.hidden_size,
            cfg.vocab_size,
            cfg.lm_head_bias,
            mapper.set_nm_device(vb.pp("lm_head"), normal_loading_metadata.loading_isq),
        )?;
        Ok(Self {
            embed_tokens,
            layers,
            norm,
            lm_head: Arc::new(UnquantLinear::new(QuantMethodConfig::Unquantized(lm_head))?),
            device: normal_loading_metadata.real_device,
            cache: Cache::new(cfg.num_hidden_layers, false),
            max_seq_len: cfg.max_position_embeddings,
            mapper,
            sliding_window: cfg.sliding_window,
            cfg: ModelConfigMetadata {
                num_layers: cfg.num_hidden_layers,
                hidden_size: cfg.hidden_size,
                num_kv_heads: cfg.num_key_value_heads,
                num_attn_heads: cfg.num_attention_heads,
                sliding_window: cfg.sliding_window,
                head_dim: None,
            },
        })
    }

    pub fn forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        position_ids: &[usize],
        context_lens: Vec<(usize, usize)>,
        mut metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
        flash_params: &FlashParams,
    ) -> Result<Tensor> {
        let mut xs = self.embed_tokens.forward(input_ids)?;
        let mut cache = self.cache.lock();
        let attention_mask = CausalMasker.make_causal_mask_with_sliding_window_as_attn_bias(
            input_ids,
            metadata
                .as_ref()
                .map(|(_, _)| &seqlen_offsets as &dyn PastKvLenCache)
                .unwrap_or(&*cache as &dyn PastKvLenCache),
            self.sliding_window,
            xs.dtype(),
            self.layers[0].self_attn.num_heads,
        )?;

        for (i, layer) in self.layers.iter().enumerate() {
            xs = self.mapper.map(xs, i)?;
            xs = layer.forward(
                &xs,
                attention_mask
                    .as_ref()
                    .map(|m| m.to_device(xs.device()).unwrap())
                    .as_ref(),
                seqlen_offsets,
                position_ids,
                &mut cache[i],
                metadata
                    .as_mut()
                    .map(|(kv_cache, metadata)| (kv_cache[i].clone(), &mut **metadata)),
                flash_params,
            )?
        }
        let xs = xs.to_device(&self.device)?;
        let mut xs = xs.apply(&self.norm)?;
        if let Some(t) = self.lm_head.quantized_act_type() {
            xs = xs.to_dtype(t)?;
        }
        extract_logits(&MatMul.qmethod_matmul(&xs, &*self.lm_head)?, context_lens)
    }
}

impl IsqModel for Model {
    fn get_layers(
        &mut self,
    ) -> (
        Vec<(&mut Arc<dyn QuantMethod>, Option<usize>)>,
        &dyn DeviceMapper,
    ) {
        let mut tensors = Vec::new();
        tensors.push((&mut self.lm_head, None));
        for (i, layer) in self.layers.iter_mut().enumerate() {
            tensors.push((&mut layer.self_attn.q_proj, Some(i)));
            tensors.push((&mut layer.self_attn.k_proj, Some(i)));
            tensors.push((&mut layer.self_attn.v_proj, Some(i)));
            tensors.push((&mut layer.self_attn.o_proj, Some(i)));
            tensors.push((&mut layer.mlp.gate, Some(i)));
            for expert in &mut layer.mlp.experts {
                tensors.push((&mut expert.w1, Some(i)));
                tensors.push((&mut expert.w2, Some(i)));
                tensors.push((&mut expert.w3, Some(i)));
            }
        }
        (tensors, &*self.mapper)
    }
}

impl NormalModel for Model {
    fn forward(
        &self,
        input_ids: &Tensor,
        seqlen_offsets: &[usize],
        _start_offsets_kernel: Tensor,
        context_lens: Vec<(usize, usize)>,
        position_ids: Vec<usize>,
        metadata: Option<(Vec<(Tensor, Tensor)>, &mut PagedAttentionInputMetadata)>,
        flash_params: &FlashParams,
    ) -> Result<Tensor> {
        self.forward(
            input_ids,
            seqlen_offsets,
            &position_ids,
            context_lens,
            metadata,
            flash_params,
        )
    }
    fn xlora_forward(
        &self,
        _input_ids: &Tensor,
        _input_ids_full: &Tensor,
        _seqlen_offsets: &[usize],
        _seqlen_offsets_full: &[usize],
        _start_offsets_kernel: Tensor,
        _start_offsets_kernel_full: Tensor,
        _no_kv_cache: bool,
        _non_granular_state: &Option<crate::xlora_models::NonGranularState>,
        _context_lens: Vec<(usize, usize)>,
        _position_ids: Vec<usize>,
        _flash_params: &FlashParams,
        _flash_params_full: &FlashParams,
    ) -> Result<Tensor> {
        unimplemented!()
    }
    fn cache(&self) -> &Cache {
        &self.cache
    }
    fn device(&self) -> &Device {
        &self.device
    }
    fn is_xlora(&self) -> bool {
        false
    }
    fn max_seq_len(&self) -> usize {
        self.max_seq_len
    }
    fn config(&self) -> &ModelConfigMetadata {
        &self.cfg
    }
}

impl AnyMoeBaseModelMixin for Model {}
