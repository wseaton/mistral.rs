#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::{
    collections::HashMap,
    f32::consts::PI,
    ops::Mul,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use candle_core::{
    quantized::{QMatMul, QTensor},
    DType, Device, IndexOp, Result, Shape, Tensor, D,
};
use candle_nn::{Linear, Module, VarBuilder};
use mistralrs_quant::QuantMethod;
use serde::Deserialize;

pub use crate::layers_masker::CausalMasker;
pub use crate::layers_utils::{flash_attn, repeat_kv};
use crate::{
    cublaslt::CUBLASLT_HANDLE, gguf::Content, models::llama, pipeline::Phi3RopeScaling,
    INHIBIT_GEMM_F16,
};

#[derive(Debug, Clone)]
pub struct RmsNorm {
    eps: f64,
    weight: Tensor,
}

impl RmsNorm {
    pub fn new(size: usize, eps: f64, vb: VarBuilder) -> Result<Self> {
        let inner = candle_nn::rms_norm_non_quant(size, eps, vb)?;
        let w = inner.inner().weight().clone();
        Ok(Self { eps, weight: w })
    }

    /// Gemma uses weight + 1.0
    pub fn new_gemma(size: usize, eps: f64, vb: VarBuilder) -> Result<Self> {
        let inner = candle_nn::rms_norm_non_quant(size, eps, vb)?;
        let w = (inner.inner().weight().clone() + 1.0)?;
        Ok(Self { eps, weight: w })
    }

    pub fn from_w(w: Tensor, eps: f64) -> Result<Self> {
        Ok(Self { eps, weight: w })
    }
}

impl Module for RmsNorm {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        candle_nn::ops::rms_norm(&x.contiguous()?, &self.weight, self.eps as f32)
    }
}

#[derive(Debug, Clone)]
pub struct QRmsNorm {
    eps: f64,
    weight: Tensor,
}

impl QRmsNorm {
    pub fn new(scale: QTensor, eps: f32) -> Result<Self> {
        let scale = scale.dequantize(&scale.device())?;
        Ok(Self {
            eps: eps as f64,
            weight: scale,
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        candle_nn::ops::rms_norm(&x.contiguous()?, &self.weight, self.eps as f32)
    }
}

/// RoPE supporting LongRope
#[derive(Debug, Clone)]
pub struct PhiRotaryEmbedding {
    short_sin: Tensor,
    short_cos: Tensor,
    long_cos: Option<Tensor>,
    long_sin: Option<Tensor>,
    original_max_position_embeddings: usize,
}

#[derive(Debug, Clone)]
enum ScaledRopeType {
    Su,
    Yarn,
}

impl FromStr for ScaledRopeType {
    type Err = candle_core::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "su" | "longrope" => Ok(Self::Su),
            "yarn" => Ok(Self::Yarn),
            _ => Err(candle_core::Error::Msg(
                "Expected either `su` or `yarn` scaled RoPE type.".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct ScaledRopeParams {
    short_factor: Vec<f64>,
    long_factor: Vec<f64>,
    scaling_type: ScaledRopeType,
}

pub struct PhiRopeConfig {
    pub rope_scaling: Option<HashMap<String, Phi3RopeScaling>>,
    pub max_position_embeddings: usize,
    pub original_max_position_embeddings: usize,
    pub rope_theta: f64,
    pub head_dim: usize,
}

impl PhiRotaryEmbedding {
    pub fn new(dtype: DType, cfg: impl Into<PhiRopeConfig>, dev: &Device) -> Result<Self> {
        let cfg: PhiRopeConfig = cfg.into();
        let scaled_params = cfg.rope_scaling.as_ref().map(|r| ScaledRopeParams {
            short_factor: r["short_factor"].clone().0.left().unwrap(),
            long_factor: r["long_factor"].clone().0.left().unwrap(),
            scaling_type: r["type"].clone().0.right().unwrap().parse().unwrap(),
        });
        let max_seq_len = cfg.max_position_embeddings;
        let dim = cfg.head_dim;

        if let Some(scaled_params) = scaled_params {
            // Calculate scale
            let scale =
                cfg.max_position_embeddings as f64 / cfg.original_max_position_embeddings as f64;
            let scaling_factor = if scale <= 1.0 {
                1.0
            } else {
                match scaled_params.scaling_type {
                    ScaledRopeType::Su => (1.0
                        + scale.ln() / (cfg.original_max_position_embeddings as f64).ln())
                    .sqrt(),
                    ScaledRopeType::Yarn => 0.1 * scale.ln() + 1.0,
                }
            };

            // Calculate inv freqs for short, long
            let inv_freq_long = (0..dim)
                .step_by(2)
                .enumerate()
                .map(|(k, i)| {
                    (1f64
                        / (scaled_params.long_factor[k]
                            * cfg.rope_theta.powf(i as f64 / dim as f64)))
                        as f32
                })
                .collect::<Vec<_>>();
            let inv_freq_short = (0..dim)
                .step_by(2)
                .enumerate()
                .map(|(k, i)| {
                    (1f64
                        / (scaled_params.short_factor[k]
                            * cfg.rope_theta.powf(i as f64 / dim as f64)))
                        as f32
                })
                .collect::<Vec<_>>();
            let inv_freq_len = inv_freq_long.len();

            let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
                .to_dtype(DType::F32)?
                .reshape((max_seq_len, 1))?;

            // Calculate sin,cos for long
            let inv_freq_long = Tensor::from_vec(inv_freq_long, (1, inv_freq_len), dev)?;
            let freqs_long = t.matmul(&inv_freq_long)?;
            let long_sin = freqs_long.sin()?.mul(scaling_factor)?.to_dtype(dtype)?;
            let long_cos = freqs_long.cos()?.mul(scaling_factor)?.to_dtype(dtype)?;

            // Calculate sin,cos for short
            let inv_freq_short =
                Tensor::from_vec(inv_freq_short, (1, inv_freq_len), dev)?.to_dtype(DType::F32)?;
            let freqs_short = t.matmul(&inv_freq_short)?;
            let short_sin = freqs_short.sin()?.mul(scaling_factor)?.to_dtype(dtype)?;
            let short_cos = freqs_short.cos()?.mul(scaling_factor)?.to_dtype(dtype)?;

            Ok(Self {
                short_cos,
                short_sin,
                long_cos: Some(long_cos),
                long_sin: Some(long_sin),
                original_max_position_embeddings: cfg.original_max_position_embeddings,
            })
        } else {
            let inv_freq: Vec<_> = (0..dim)
                .step_by(2)
                .map(|i| 1f32 / cfg.rope_theta.powf(i as f64 / dim as f64) as f32)
                .collect();
            let inv_freq_len = inv_freq.len();
            let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?;
            let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
                .to_dtype(DType::F32)?
                .reshape((max_seq_len, 1))?;
            let freqs = t.matmul(&inv_freq)?;
            let sin = freqs.sin()?.to_dtype(dtype)?;
            let cos = freqs.cos()?.to_dtype(dtype)?;
            Ok(Self {
                short_cos: cos,
                short_sin: sin,
                long_cos: None,
                long_sin: None,
                original_max_position_embeddings: cfg.original_max_position_embeddings,
            })
        }
    }

    /// Returns (sin, cos) taking into account LongRope
    fn get_long_or_short_sin_cos(&self, position_ids: &[usize]) -> (&Tensor, &Tensor) {
        if self.long_cos.is_none() {
            return (&self.short_sin, &self.short_cos);
        }
        let seq_len = position_ids.iter().max().unwrap() + 1;
        if seq_len > self.original_max_position_embeddings {
            (
                self.long_sin.as_ref().unwrap(),
                self.long_cos.as_ref().unwrap(),
            )
        } else {
            (&self.short_sin, &self.short_cos)
        }
    }

    pub fn forward(
        &self,
        q: &Tensor,
        k: &Tensor,
        seqlen_offsets: &[usize],
        position_ids: &[usize],
    ) -> Result<(Tensor, Tensor)> {
        let (_b_sz, _h, seq_len, _n_embd) = q.dims4()?;
        let mut q_embeds = Vec::new();
        let mut k_embeds = Vec::new();
        let (sin, cos) = self.get_long_or_short_sin_cos(position_ids);
        for (i, offset) in seqlen_offsets.iter().enumerate() {
            let cos = cos.narrow(0, *offset, seq_len)?;
            let sin = sin.narrow(0, *offset, seq_len)?;
            let q_embed =
                candle_nn::rotary_emb::rope(&q.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
            let k_embed =
                candle_nn::rotary_emb::rope(&k.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
            q_embeds.push(q_embed);
            k_embeds.push(k_embed);
        }
        Ok((Tensor::cat(&q_embeds, 0)?, Tensor::cat(&k_embeds, 0)?))
    }
}

/// RoPE for Llama3
#[derive(Debug, Clone)]
pub enum Llama3RotaryEmbedding {
    Llama3 {
        sin: Tensor,
        cos: Tensor,
        is_gptx: bool,
    },
    Default(RotaryEmbedding),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub enum Llama3RopeType {
    #[serde(rename = "llama3")]
    Llama3,
    #[default]
    #[serde(rename = "default")]
    Default,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Llama3RopeConfig {
    pub factor: f32,
    pub low_freq_factor: f32,
    pub high_freq_factor: f32,
    pub original_max_position_embeddings: usize,
    pub rope_type: Llama3RopeType,
}

fn calculate_default_inv_freq(cfg: &llama::Config) -> Vec<f32> {
    let head_dim = cfg.hidden_size / cfg.num_attention_heads;
    (0..head_dim)
        .step_by(2)
        .map(|i| 1f32 / cfg.rope_theta.powf(i as f32 / head_dim as f32))
        .collect()
}

// https://github.com/huggingface/transformers/blob/1392a6867f40a55dfabaf306745c67627598b1af/src/transformers/modeling_rope_utils.py#L298
impl Llama3RotaryEmbedding {
    pub fn new(dtype: DType, cfg: &llama::Config, dev: &Device, is_gpt_neox: bool) -> Result<Self> {
        match &cfg.rope_scaling {
            None
            | Some(Llama3RopeConfig {
                rope_type: Llama3RopeType::Default,
                ..
            }) => Ok(Self::Default(RotaryEmbedding::new(
                cfg.rope_theta,
                cfg.hidden_size / cfg.num_attention_heads,
                cfg.max_position_embeddings,
                dev,
                is_gpt_neox,
                dtype,
            )?)),
            Some(rope_scaling) => {
                let low_freq_wavelen = rope_scaling.original_max_position_embeddings as f32
                    / rope_scaling.low_freq_factor;
                let high_freq_wavelen = rope_scaling.original_max_position_embeddings as f32
                    / rope_scaling.high_freq_factor;

                let inv_freq = calculate_default_inv_freq(cfg)
                    .into_iter()
                    .map(|freq| {
                        let wavelen = 2. * PI / freq;
                        if wavelen < high_freq_wavelen {
                            freq
                        } else if wavelen > low_freq_wavelen {
                            freq / rope_scaling.factor
                        } else {
                            let smooth = (rope_scaling.original_max_position_embeddings as f32
                                / wavelen
                                - rope_scaling.low_freq_factor)
                                / (rope_scaling.high_freq_factor - rope_scaling.low_freq_factor);
                            (1. - smooth) * freq / rope_scaling.factor + smooth * freq
                        }
                    })
                    .collect::<Vec<_>>();
                let inv_freq_len = inv_freq.len();
                let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?;

                let t = Tensor::arange(0u32, cfg.max_position_embeddings as u32, dev)?
                    .to_dtype(DType::F32)?
                    .reshape((cfg.max_position_embeddings, 1))?;
                let freqs = t.matmul(&inv_freq)?;
                let sin = freqs.sin()?.to_dtype(dtype)?;
                let cos = freqs.cos()?.to_dtype(dtype)?;
                Ok(Self::Llama3 {
                    sin,
                    cos,
                    is_gptx: is_gpt_neox,
                })
            }
        }
    }

    pub fn forward(
        &self,
        positions: &[usize],
        positions_kernel: &Tensor,
        q: &mut Tensor,
        k: &mut Tensor,
        b_sz: usize,
    ) -> Result<()> {
        match self {
            Self::Llama3 { sin, cos, is_gptx } => {
                let (b_sz_seq_len, h, n_embd) = q.dims3()?;
                *q = q
                    .reshape((b_sz, b_sz_seq_len / b_sz, h, n_embd))?
                    .transpose(1, 2)?;
                let (b_sz_seq_len, h, n_embd) = k.dims3()?;
                *k = k
                    .reshape((b_sz, b_sz_seq_len / b_sz, h, n_embd))?
                    .transpose(1, 2)?;

                let (_b_sz, _h, seq_len, _n_embd) = q.dims4()?;
                let mut q_embeds = Vec::new();
                let mut k_embeds = Vec::new();
                for (i, offset) in positions.iter().enumerate() {
                    let cos = cos.narrow(0, *offset, seq_len)?;
                    let sin = sin.narrow(0, *offset, seq_len)?;
                    let rope = if *is_gptx {
                        candle_nn::rotary_emb::rope
                    } else {
                        candle_nn::rotary_emb::rope_i
                    };
                    let q_embed = rope(&q.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
                    let k_embed = rope(&k.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
                    q_embeds.push(q_embed);
                    k_embeds.push(k_embed);
                }
                *q = Tensor::cat(&q_embeds, 0)?;
                *k = Tensor::cat(&k_embeds, 0)?;
                Ok(())
            }
            Self::Default(rope) => rope.forward(positions, positions_kernel, q, k, b_sz),
        }
    }
}

/// Matrix multiplication, configurable to be via f16 (to use the faster GEMM kernels) optionally.
pub struct MatMul;

/// Set the matmuls to go via f16
pub(crate) static USE_MATMUL_VIA_F16: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_use_matmul_via_f16(via_f16: bool) {
    if !INHIBIT_GEMM_F16.load(Ordering::Relaxed) {
        USE_MATMUL_VIA_F16.store(via_f16, Ordering::Relaxed)
    }
}
pub fn get_use_matmul_via_f16() -> bool {
    USE_MATMUL_VIA_F16.load(Ordering::Relaxed)
}

impl MatMul {
    /// Compute matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    pub fn matmul(&self, a: &Tensor, b: &Tensor) -> Result<Tensor> {
        if !get_use_matmul_via_f16() {
            return a.matmul(b);
        }
        let original_dtype = a.dtype();
        a.to_dtype(DType::F16)?
            .matmul(&b.to_dtype(DType::F16)?)?
            .to_dtype(original_dtype)
    }

    /// Compute matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    /// The result will be divided by the `scale` parameter in an affine division.
    pub fn matmul_affine_div(&self, a: &Tensor, b: &Tensor, scale: f64) -> Result<Tensor> {
        // TODO(EricLBuehler): Optimize this by using the gemm parameter
        self.matmul(a, b)? / scale
    }

    /// Compute quantized matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    pub fn qmatmul(&self, x: &Tensor, matmul: &QMatMul) -> Result<Tensor> {
        if get_use_matmul_via_f16() {
            matmul.forward_via_f16(x)
        } else {
            matmul.forward(x)
        }
    }

    /// Compute quantized matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    pub fn qmethod_matmul(&self, x: &Tensor, matmul: &dyn QuantMethod) -> Result<Tensor> {
        if get_use_matmul_via_f16() {
            matmul.forward_via_half(x)
        } else {
            matmul.forward(x)
        }
    }
}

/// Computes softmax(QK^T*sqrt(d_k))V
fn naive_sdpa(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    head_dim: usize,
    mask: Option<&Tensor>,
) -> Result<Tensor> {
    let att = MatMul.matmul_affine_div(
        &q.contiguous()?,
        &k.t()?.contiguous()?,
        (head_dim as f64).sqrt(),
    )?;

    let att = match mask {
        Some(m) => att.broadcast_add(m)?,
        None => att,
    };
    let att = candle_nn::ops::softmax_last_dim(&att)?;
    // Convert to contiguous as matmul doesn't support strided vs for now.
    MatMul.matmul(&att, &v.contiguous()?)
}

pub struct ScaledDotProductAttention;

impl ScaledDotProductAttention {
    /// Computes softmax(QK^T*sqrt(d_k))V
    ///
    /// The attention implementation is dispatched as follows:
    /// 1) If `use_flash_attn == true`, use a flash attention V2 kernel
    /// 2) If using CUDA and the cuBLASLt kernel is initialized, then it will use an optimized version.
    /// 3) Otherwise, use the "naive" SDPA implementation.
    #[allow(unused_variables, clippy::too_many_arguments)]
    pub fn run_attention(
        &self,
        q: &Tensor,
        k: &Tensor,
        v: &Tensor,
        n_attn_heads: usize,
        head_dim: usize,
        mask: Option<&Tensor>,
        use_flash_attn: bool,
        b_sz: usize,
        seq_len: usize,
    ) -> Result<Tensor> {
        if use_flash_attn {
            // flash-attn expects (b_sz, seq_len, nheads, head_dim)
            let q = q.transpose(1, 2)?;
            let k = k.transpose(1, 2)?;
            let v = v.transpose(1, 2)?;
            let softmax_scale = 1f32 / (head_dim as f32).sqrt();
            return flash_attn(&q, &k, &v, softmax_scale, seq_len > 1)?.transpose(1, 2);
        }
        if let (Device::Cuda(_), Some(cublaslt)) = (q.device(), *CUBLASLT_HANDLE.lock().unwrap()) {
            if !get_use_matmul_via_f16() {
                #[cfg(feature = "cuda")]
                {
                    // cuBLASLt batch matmul implementation requires inputs to be dims3
                    let k = k.flatten(0, 1)?;
                    let q = q.flatten(0, 1)?;
                    let v = v.flatten(0, 1)?;
                    let attention_bias = mask.map(|mask| mask.flatten(0, 1)).transpose()?;

                    // If attention_bias is set, we fuse the add by giving it as the output matrix
                    // and setting beta to 1.0
                    let beta = match attention_bias.is_some() {
                        true => Some(1.0),
                        false => None,
                    };

                    // Batch matrix multiplication
                    // Fuse softmax scale and attention_bias add
                    let attention_scores = cublaslt.batch_matmul(
                        &k,
                        &q,
                        attention_bias.as_ref(),
                        Some((1.0 / (head_dim as f64).sqrt()) as f32),
                        beta,
                        None,
                        None,
                    )?;
                    let attention_probs = candle_nn::ops::softmax_last_dim(&attention_scores)?;

                    let context_layer = cublaslt.batch_matmul(
                        &v.t()?.contiguous()?,
                        &attention_probs,
                        // We save one allocation
                        Some(&q),
                        None,
                        None,
                        None,
                        None,
                    )?;

                    // Reshape to dims4
                    context_layer.reshape((b_sz, n_attn_heads, seq_len, head_dim))
                }
                #[cfg(not(feature = "cuda"))]
                {
                    candle_core::bail!("`cuda` feature is not enabled")
                }
            } else {
                // Use the f16 kernels here if quantized (ISQ or GGML), and a large enough prompt
                naive_sdpa(q, k, v, head_dim, mask)
            }
        } else {
            naive_sdpa(q, k, v, head_dim, mask)
        }
    }
}

/// Linear layer with fused bias matmul.
#[derive(Debug, Clone)]
pub struct FusedBiasLinear {
    pub(crate) w: Tensor,
    pub(crate) b: Tensor,
}

impl TryFrom<Linear> for FusedBiasLinear {
    type Error = candle_core::Error;

    fn try_from(x: Linear) -> Result<Self> {
        if let Some(bias) = x.bias() {
            Ok(Self {
                w: x.weight().clone(),
                b: bias.clone(),
            })
        } else {
            candle_core::bail!("`FusedBiasLinear` expects a Linear layer with bias.")
        }
    }
}

impl Module for FusedBiasLinear {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let w = match *x.dims() {
            [b1, b2, _, _] => self.w.broadcast_left((b1, b2))?,
            [bsize, _, _] => self.w.broadcast_left(bsize)?,
            _ => self.w.clone(),
        };
        let mut tgt_shape = x.dims().to_vec();
        tgt_shape[x.dims().len() - 1] = w.dim(D::Minus2)?;
        let b = self.b.broadcast_as(Shape::from_dims(&tgt_shape))?;

        if let (Device::Cuda(_), Some(cublaslt)) = (x.device(), *CUBLASLT_HANDLE.lock().unwrap()) {
            cublaslt
                .batch_matmul(
                    x,
                    &w,
                    Some(&b.t()?.contiguous()?),
                    None,
                    Some(1.0),
                    None,
                    None,
                )?
                .t()
        } else {
            x.matmul(&w.t()?)? + b
        }
    }
}

#[derive(Debug, Clone)]
pub struct QLinear {
    inner: QMatMul,
    bias: Option<Tensor>,
    dtype: DType,
}

impl QLinear {
    pub fn new<R: std::io::Read + std::io::Seek>(
        ct: &mut Content<'_, R>,
        name: &str,
        device: &Device,
    ) -> Result<Self> {
        let w = ct.tensor(&format!("{name}.weight"), device)?;
        let b = ct.tensor(&format!("{name}.bias"), device)?;
        let inner = QMatMul::from_qtensor(w)?;
        let bias = b.dequantize(device)?;
        Ok(Self {
            inner,
            bias: Some(bias),
            dtype: DType::F32,
        })
    }

    pub fn from_linear(linear: Linear) -> Self {
        Self {
            inner: QMatMul::Tensor(linear.weight().clone()),
            bias: linear.bias().cloned(),
            dtype: linear.weight().dtype(),
        }
    }

    pub fn from_parts(w: Tensor, b: Option<Tensor>) -> Self {
        let dtype = w.dtype();
        Self {
            inner: QMatMul::Tensor(w),
            bias: b,
            dtype,
        }
    }

    pub fn from_qparts(w: QTensor, b: Option<Tensor>) -> Self {
        if let Some(ref b) = b {
            assert_eq!(b.dtype(), DType::F32);
        }
        Self {
            inner: QMatMul::QTensor(Arc::new(w)),
            bias: b,
            dtype: DType::F32,
        }
    }

    pub fn from_old_and_qmatmul(inner: QMatMul, old: &Self) -> Self {
        Self {
            inner,
            bias: old.bias.clone(),
            dtype: old.dtype,
        }
    }

    pub fn inner(&mut self) -> &mut QMatMul {
        &mut self.inner
    }

    pub fn inner_ref(&self) -> &QMatMul {
        &self.inner
    }

    pub fn is_quant(&self) -> bool {
        matches!(self.inner, QMatMul::QTensor(_))
    }

    pub fn bias(&self) -> Option<&Tensor> {
        self.bias.as_ref()
    }

    pub fn bias_mut(&mut self) -> Option<&mut Tensor> {
        self.bias.as_mut()
    }
}

impl Module for QLinear {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let xs = if self.is_quant() {
            xs.to_dtype(DType::F32)?
        } else {
            xs.clone()
        };
        let forward_fn = if !get_use_matmul_via_f16() {
            QMatMul::forward
        } else {
            QMatMul::forward_via_f16
        };
        if let Some(bias) = &self.bias {
            forward_fn(&self.inner, &xs)?
                .broadcast_add(bias)?
                .to_dtype(self.dtype)
        } else {
            forward_fn(&self.inner, &xs)?.to_dtype(self.dtype)
        }
    }
}

#[derive(Debug, Clone)]
pub struct RotaryEmbedding(candle_nn::RotaryEmbedding);

impl RotaryEmbedding {
    pub fn new(
        base: f32,
        head_dim: usize,
        max_position_embeddings: usize,
        device: &Device,
        is_gpt_neox: bool,
        dtype: DType,
    ) -> Result<Self> {
        Ok(Self(candle_nn::RotaryEmbedding::new(
            base,
            head_dim,
            max_position_embeddings,
            device,
            is_gpt_neox,
            dtype,
        )?))
    }

    pub fn new_partial(
        base: f32,
        head_dim: usize,
        rot_dim: usize,
        max_position_embeddings: usize,
        device: &Device,
        is_gpt_neox: bool,
        dtype: DType,
    ) -> Result<Self> {
        Ok(Self(candle_nn::RotaryEmbedding::new_partial(
            base,
            head_dim,
            rot_dim,
            max_position_embeddings,
            device,
            is_gpt_neox,
            dtype,
        )?))
    }

    pub fn forward(
        &self,
        positions: &[usize],
        positions_kernel: &Tensor,
        q: &mut Tensor,
        k: &mut Tensor,
        b_sz: usize,
    ) -> Result<()> {
        self.0.forward(positions, positions_kernel, q, k, b_sz)
    }
}

mod tests {

    #[test]
    fn fused_bias_linear() {
        use candle_core::{DType, Device, IndexOp, Tensor};
        use candle_nn::{Linear, Module};

        use crate::cublaslt::setup_cublas_lt_wrapper;
        use crate::layers::FusedBiasLinear;

        const IN: usize = 1921;
        const OUT: usize = 4096;
        const INNER: usize = 1024;

        let dev = Device::cuda_if_available(0).unwrap();
        setup_cublas_lt_wrapper();

        let inner_dtype = if dev.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };

        let w = Tensor::arange(0f32, (OUT * IN) as f32, &dev)
            .unwrap()
            .to_dtype(inner_dtype)
            .unwrap()
            .reshape((OUT, IN))
            .unwrap();
        let b = Tensor::arange(0f32, OUT as f32, &dev)
            .unwrap()
            .to_dtype(inner_dtype)
            .unwrap()
            .reshape((OUT,))
            .unwrap();

        let xs = Tensor::arange(0f32, (INNER * IN) as f32, &dev)
            .unwrap()
            .to_dtype(inner_dtype)
            .unwrap()
            .reshape((1, INNER, IN))
            .unwrap();

        let lin = Linear::new(w.clone(), Some(b.clone()));
        let truth_out = lin.forward(&xs).unwrap();
        let truth_y = truth_out
            .to_dtype(DType::F32)
            .unwrap()
            .to_vec3::<f32>()
            .unwrap();

        let fused = FusedBiasLinear { w, b };
        let fused_out = fused.forward(&xs).unwrap();
        let fused_y = fused_out
            .to_dtype(DType::F32)
            .unwrap()
            .to_vec3::<f32>()
            .unwrap();

        assert_eq!(truth_out.shape(), fused_out.shape());
        if truth_y != fused_y {
            panic!(
                "Truth does not match fused kernel. Diff fused - truth:\n{:#?}",
                &(&fused_out - &truth_out)
                    .unwrap()
                    .i((0, 5..10, 0..5))
                    .unwrap()
                    .to_dtype(DType::F32)
                    .unwrap()
                    .to_vec2::<f32>()
            )
        }
    }
}
