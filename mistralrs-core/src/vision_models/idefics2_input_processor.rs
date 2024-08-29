#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::{any::Any, num::NonZeroUsize, sync::Arc};

use candle_core::{Device, Result, Tensor};
use image::{DynamicImage, GenericImageView};
use indexmap::IndexMap;
use mistralrs_vision::{ApplyTransforms, Normalize, Rescale, ToTensorNoNorm, Transforms};
use tokenizers::Tokenizer;
use tracing::warn;

use crate::{
    pipeline::{
        apply_chat_template,
        text_models_inputs_processor::{
            self, get_completion_input, get_prompt_input, PagedAttentionMeta,
        },
        InputProcessorOutput, InputsProcessor, InputsProcessorType, MessagesAction, Processor,
    },
    sequence::Sequence,
    vision_models::ModelInputs,
    MessageContent, Pipeline, Tool,
};

use super::{
    image_processor::{ImagePreProcessor, PreprocessedImages},
    preprocessor_config::{PreProcessorConfig, ToFilter},
    processor_config::ProcessorConfig,
};

// Input processor
pub struct Idefics2ImageProcessor;
// Processor
pub struct Idefics2Processor {
    config: ProcessorConfig,
    preprocessor_config: PreProcessorConfig,
    fake_image_token: &'static str,
    image_token: &'static str,
}

impl Idefics2Processor {
    pub fn new(config: ProcessorConfig, preprocessor_config: PreProcessorConfig) -> Self {
        Self {
            config,
            preprocessor_config,
            fake_image_token: "<fake_token_around_image>",
            image_token: "<image>",
        }
    }
}

impl Processor for Idefics2Processor {
    fn process(
        &self,
        pipeline: &dyn Pipeline,
        messages: Vec<IndexMap<String, MessageContent>>,
        add_generation_prompt: bool,
        tools: Vec<Tool>,
    ) -> anyhow::Result<Vec<u32>> {
        let mut prompt = apply_chat_template(
            pipeline,
            messages,
            add_generation_prompt,
            self.template_action(),
            tools,
        )?;

        let mut image_str = format!(
            "{}{}{}",
            self.fake_image_token,
            self.image_token.repeat(
                self.config
                    .image_seq_len
                    .expect("Idefics 2 model needs `image_seq_len`")
            ),
            self.fake_image_token
        );
        if self
            .preprocessor_config
            .do_image_splitting
            .is_some_and(|x| x)
        {
            // 4 patches + 1 original
            image_str = image_str.repeat(5);
        }

        prompt = prompt.replace(self.image_token, &image_str);
        // Deal with any adjacent images.
        prompt = prompt.replace(
            &format!("{}{}", self.fake_image_token, self.fake_image_token),
            self.fake_image_token,
        );

        let encoding = pipeline
            .tokenizer()
            .encode(prompt, true)
            .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        Ok(encoding.get_ids().to_vec())
    }

    fn inputs_processor(&self) -> Arc<dyn InputsProcessor> {
        Arc::new(Idefics2ImageProcessor)
    }

    fn get_special_tokens(&self) -> &[&'static str] {
        &["<fake_token_around_image>", "<image>", "<end_of_utterance>"]
    }

    fn template_action(&self) -> MessagesAction {
        MessagesAction::Keep
    }
}

impl InputsProcessor for Idefics2ImageProcessor {
    fn get_type(&self) -> InputsProcessorType {
        InputsProcessorType::Vision
    }
    fn process_inputs(
        &self,
        _: Arc<Tokenizer>,
        input_seqs: &mut [&mut Sequence],
        is_prompt: bool,
        is_xlora: bool,
        device: &Device,
        no_kv_cache: bool,
        last_n_context_len: Option<(usize, usize)>,
        other_config: Option<Arc<dyn Any>>,
        mut paged_attn_metadata: Option<PagedAttentionMeta<'_>>,
        prompt_batchsize: Option<NonZeroUsize>,
    ) -> Box<dyn Iterator<Item = anyhow::Result<InputProcessorOutput>>> {
        if is_xlora {
            return Box::new(std::iter::once(Err(anyhow::Error::msg(
                "Cannot make inputs for X-LoRA vision model.",
            ))));
        }
        if no_kv_cache {
            return Box::new(std::iter::once(Err(anyhow::Error::msg(
                "Vision model must have kv cache.",
            ))));
        }
        // TODO(EricLBuehler): support this? Would require some handling of image tokens.
        if prompt_batchsize.is_some() {
            warn!("`prompt_batchsize` is set. Idefics 2 does not support prompt batching.");
        }

        let text_models_inputs_processor::InnerInputProcessorOutput {
            inputs:
                text_models_inputs_processor::InputMetadata {
                    input,
                    positions,
                    positions_kernel,
                    context_lens,
                    position_ids,
                    paged_attn_meta,
                    flash_meta,
                },
            seq_indices,
        } = if is_prompt {
            get_prompt_input(
                input_seqs
                    .iter()
                    .map(|seq| seq.get_toks().to_vec())
                    .collect::<Vec<_>>(),
                input_seqs,
                device,
                last_n_context_len,
                paged_attn_metadata.as_mut(),
                None, // TODO: evaluate if it is possible to batch this
            )
            .nth(0)
            .unwrap()
            .unwrap()
        } else {
            get_completion_input(
                input_seqs
                    .iter()
                    .map(|seq| seq.get_toks().to_vec())
                    .collect::<Vec<_>>(),
                input_seqs,
                device,
                no_kv_cache,
                last_n_context_len,
                paged_attn_metadata.as_mut(),
                None, // TODO: evaluate if it is possible to batch this
            )
            .nth(0)
            .unwrap()
            .unwrap()
        };
        let config = other_config.expect("Need a PreProcessorConfig config.");
        let config: &PreProcessorConfig = config.downcast_ref().expect("Downcast failed.");

        let (pixel_values, pixel_attention_mask) = if is_prompt {
            let mut pixel_values_accum = Vec::new();
            let mut pixel_attention_mask_accum = Vec::new();
            for seq in input_seqs.iter_mut() {
                let PreprocessedImages {
                    pixel_values,
                    pixel_attention_mask,
                    image_sizes: _,
                    num_img_tokens: _,
                } = self
                    .preprocess(
                        seq.take_images()
                            .expect("Need to have images by this point."),
                        config,
                        device,
                    )
                    .expect("Preprocessing failed");
                pixel_values_accum.push(pixel_values.unsqueeze(0).unwrap());
                pixel_attention_mask_accum
                    .push(pixel_attention_mask.unwrap().unsqueeze(0).unwrap());
            }
            (
                Some(Tensor::cat(&pixel_values_accum, 0).unwrap()),
                Some(Tensor::cat(&pixel_attention_mask_accum, 0).unwrap()),
            )
        } else {
            (None, None)
        };

        let inputs: Box<dyn Any> = Box::new(ModelInputs {
            input_ids: input,
            seqlen_offsets: positions,
            seqlen_offsets_kernel: positions_kernel,
            context_lens,
            position_ids,
            pixel_values,
            model_specific_args: Box::new(pixel_attention_mask),
            paged_attn_meta,
            flash_meta,
        });
        Box::new(std::iter::once(Ok(InputProcessorOutput {
            inputs,
            seq_indices,
        })))
    }
}

impl ImagePreProcessor for Idefics2ImageProcessor {
    #[allow(clippy::excessive_precision)]
    const DEFAULT_MEAN: [f64; 3] = [0.48145466, 0.4578275, 0.40821073];
    #[allow(clippy::excessive_precision)]
    const DEFAULT_STD: [f64; 3] = [0.26862954, 0.26130258, 0.27577711];

    fn preprocess(
        &self,
        mut images: Vec<DynamicImage>,
        config: &PreProcessorConfig,
        device: &Device,
    ) -> Result<PreprocessedImages> {
        let mut patch_masks = Vec::new();
        let mut pixel_values = Vec::new();

        // Image splitting
        if config.do_image_splitting.is_some_and(|x| x) {
            let mut new_images = Vec::new();
            for image in images {
                let (w, h) = image.dimensions();
                let mid_w = w / 2;
                let mid_h = h / 2;
                new_images.push(image.crop_imm(0, 0, mid_w, mid_h));
                new_images.push(image.crop_imm(mid_w, 0, w, mid_h));
                new_images.push(image.crop_imm(0, mid_h, mid_w, h));
                new_images.push(image.crop_imm(mid_w, mid_h, w, h));
                new_images.push(image);
            }
            images = new_images;
        }

        for image in images.iter_mut() {
            // Resize
            if config.do_resize.is_some_and(|x| x) {
                let size = config.size.as_ref().unwrap();
                let (h, w) = if size.contains_key("shortest_edge")
                    && size.contains_key("longest_edge")
                {
                    mistralrs_vision::get_resize_image_size(
                        (image.dimensions().1 as usize, image.dimensions().0 as usize),
                        (
                            size["shortest_edge"] as usize,
                            size["longest_edge"] as usize,
                        ),
                    )
                } else if size.contains_key("height") && size.contains_key("width") {
                    (size["height"] as usize, size["width"] as usize)
                } else {
                    candle_core::bail!("Size must be a map of `shortest_edge` and `longest_edge` or `height` and `width`.");
                };

                *image = image.resize_exact(w as u32, h as u32, config.resampling.to_filter()?);
            }
        }

        let mut max_h = 0;
        let mut max_w = 0;
        for image in &images {
            let (w, h) = image.dimensions();
            if w > max_w {
                max_w = w;
            }
            if h > max_h {
                max_h = h;
            }
        }

        for image in images.iter_mut() {
            // Convert to rgb
            if config.do_convert_rgb.is_some_and(|x| x) {
                *image = DynamicImage::ImageRgb8(image.to_rgb8());
            }

            let transforms = Transforms {
                input: &ToTensorNoNorm,
                inner_transforms: &[
                    &config
                        .do_rescale
                        .is_some_and(|x| x)
                        .then_some(())
                        .map(|_| Rescale {
                            factor: config.rescale_factor,
                        }),
                    &config
                        .do_normalize
                        .is_some_and(|x| x)
                        .then_some(())
                        .map(|_| Normalize {
                            mean: config.image_mean.unwrap_or(Self::DEFAULT_MEAN).to_vec(),
                            std: config.image_std.unwrap_or(Self::DEFAULT_STD).to_vec(),
                        }),
                ],
            };

            let mut image = image.apply(transforms, device)?;
            // Pad images, calculating attention mask.
            if config.do_pad.is_some_and(|x| x) {
                let (_c, h, w) = image.dims3()?;
                let padded = mistralrs_vision::pad(&image, max_h as usize, max_w as usize)?;
                let mask = mistralrs_vision::make_pixel_mask(&padded, h, w)?;
                patch_masks.push(mask.unsqueeze(0)?);
                image = padded;
            }

            // Get pixel values
            pixel_values.push(image.unsqueeze(0)?)
        }

        Ok(PreprocessedImages {
            pixel_values: Tensor::cat(&pixel_values, 0)?,
            pixel_attention_mask: Some(Tensor::cat(&patch_masks, 0)?),
            image_sizes: None,
            num_img_tokens: None,
        })
    }
}
