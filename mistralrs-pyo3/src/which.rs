use either::Either;
use mistralrs_core::{NormalLoaderType, VisionLoaderType};
use pyo3::pyclass;

#[pyclass(eq, eq_int)]
#[derive(Debug, Clone, PartialEq)]
pub enum Architecture {
    Mistral,
    Gemma,
    Mixtral,
    Llama,
    Phi2,
    Phi3,
    Qwen2,
    Gemma2,
    Starcoder2,
    Phi3_5MoE,
}

impl From<Architecture> for NormalLoaderType {
    fn from(value: Architecture) -> Self {
        match value {
            Architecture::Gemma => Self::Gemma,
            Architecture::Llama => Self::Llama,
            Architecture::Mistral => Self::Mistral,
            Architecture::Mixtral => Self::Mixtral,
            Architecture::Phi2 => Self::Phi2,
            Architecture::Phi3 => Self::Phi3,
            Architecture::Qwen2 => Self::Qwen2,
            Architecture::Gemma2 => Self::Gemma2,
            Architecture::Starcoder2 => Self::Starcoder2,
            Architecture::Phi3_5MoE => Self::Phi3_5MoE,
        }
    }
}

#[pyclass(eq, eq_int)]
#[derive(Debug, Clone, PartialEq)]
pub enum VisionArchitecture {
    Phi3V,
    Idefics2,
    LLaVANext,
    LLaVA,
}

impl From<VisionArchitecture> for VisionLoaderType {
    fn from(value: VisionArchitecture) -> Self {
        match value {
            VisionArchitecture::Phi3V => VisionLoaderType::Phi3V,
            VisionArchitecture::Idefics2 => VisionLoaderType::Idefics2,
            VisionArchitecture::LLaVANext => VisionLoaderType::LLaVANext,
            VisionArchitecture::LLaVA => VisionLoaderType::LLaVA,
        }
    }
}

#[pyclass]
#[derive(Clone)]
pub enum Which {
    #[pyo3(constructor = (
        model_id,
        arch = None,
        tokenizer_json = None,
        topology = None,
        organization = None
    ))]
    Plain {
        model_id: String,
        arch: Option<Architecture>,
        tokenizer_json: Option<String>,
        topology: Option<String>,
        organization: Option<String>,
    },

    #[pyo3(constructor = (
        xlora_model_id,
        order,
        arch = None,
        model_id = None,
        tokenizer_json = None,
        tgt_non_granular_index = None,
        topology = None
    ))]
    XLora {
        xlora_model_id: String,
        order: String,
        arch: Option<Architecture>,
        model_id: Option<String>,
        tokenizer_json: Option<String>,
        tgt_non_granular_index: Option<usize>,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        adapters_model_id,
        order,
        arch = None,
        model_id = None,
        tokenizer_json = None,
        topology = None
    ))]
    Lora {
        adapters_model_id: String,
        order: String,
        arch: Option<Architecture>,
        model_id: Option<String>,
        tokenizer_json: Option<String>,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        tok_model_id = None,
        topology = None
    ))]
    #[allow(clippy::upper_case_acronyms)]
    GGUF {
        quantized_model_id: String,
        quantized_filename: Either<String, Vec<String>>,
        tok_model_id: Option<String>,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        xlora_model_id,
        order,
        tok_model_id = None,
        tgt_non_granular_index = None,
        topology = None
    ))]
    XLoraGGUF {
        quantized_model_id: String,
        quantized_filename: Either<String, Vec<String>>,
        xlora_model_id: String,
        order: String,
        tok_model_id: Option<String>,
        tgt_non_granular_index: Option<usize>,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        adapters_model_id,
        order,
        tok_model_id = None,
        topology = None
    ))]
    LoraGGUF {
        quantized_model_id: String,
        quantized_filename: Either<String, Vec<String>>,
        adapters_model_id: String,
        order: String,
        tok_model_id: Option<String>,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        tok_model_id,
        tokenizer_json = None,
        gqa = 1,
        topology = None
    ))]
    #[allow(clippy::upper_case_acronyms)]
    GGML {
        quantized_model_id: String,
        quantized_filename: String,
        tok_model_id: String,
        tokenizer_json: Option<String>,
        gqa: usize,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        xlora_model_id,
        order,
        tok_model_id = None,
        tokenizer_json = None,
        tgt_non_granular_index = None,
        gqa = 1,
        topology = None
    ))]
    XLoraGGML {
        quantized_model_id: String,
        quantized_filename: String,
        xlora_model_id: String,
        order: String,
        tok_model_id: Option<String>,
        tokenizer_json: Option<String>,
        tgt_non_granular_index: Option<usize>,
        gqa: usize,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        quantized_model_id,
        quantized_filename,
        adapters_model_id,
        order,
        tok_model_id = None,
        tokenizer_json = None,
        gqa = 1,
        topology = None
    ))]
    LoraGGML {
        quantized_model_id: String,
        quantized_filename: String,
        adapters_model_id: String,
        order: String,
        tok_model_id: Option<String>,
        tokenizer_json: Option<String>,
        gqa: usize,
        topology: Option<String>,
    },

    #[pyo3(constructor = (
        model_id,
        arch,
        tokenizer_json = None,
        topology = None,
    ))]
    VisionPlain {
        model_id: String,
        arch: VisionArchitecture,
        tokenizer_json: Option<String>,
        topology: Option<String>,
    },
}
