use either::Either;
use indexmap::IndexMap;
use mistralrs_core::{
    Constraint, DrySamplingParams, MessageContent, MistralRs, NormalRequest, Request,
    RequestMessage, Response, SamplingParams, TERMINATE_ALL_NEXT_STEP,
};
use once_cell::sync::Lazy;
use std::{
    io::{self, Write},
    sync::{atomic::Ordering, Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc::channel;
use tracing::{error, info};

use crate::util;

fn exit_handler() {
    std::process::exit(0);
}

fn terminate_handler() {
    TERMINATE_ALL_NEXT_STEP.store(true, Ordering::SeqCst);
}

static CTRLC_HANDLER: Lazy<Mutex<&'static (dyn Fn() + Sync)>> =
    Lazy::new(|| Mutex::new(&exit_handler));

pub async fn interactive_mode(mistralrs: Arc<MistralRs>, vision_chat: bool, throughput: bool) {
    let sender = mistralrs.get_sender().unwrap();
    let mut messages: Vec<IndexMap<String, MessageContent>> = Vec::new();
    let mut images = Vec::new();

    let sampling_params = SamplingParams {
        temperature: Some(0.1),
        top_k: Some(32),
        top_p: Some(0.1),
        min_p: Some(0.05),
        top_n_logprobs: 0,
        frequency_penalty: Some(0.1),
        presence_penalty: Some(0.1),
        max_len: Some(4096),
        stop_toks: None,
        logits_bias: None,
        n_choices: 1,
        dry_params: Some(DrySamplingParams::default()),
    };
    info!("Starting interactive loop with sampling params: {sampling_params:?}");

    // Set the handler to process exit
    *CTRLC_HANDLER.lock().unwrap() = &exit_handler;

    ctrlc::set_handler(move || CTRLC_HANDLER.lock().unwrap()())
        .expect("Failed to set CTRL-C handler for interactive mode");

    'outer: loop {
        // Set the handler to process exit
        *CTRLC_HANDLER.lock().unwrap() = &exit_handler;

        let request_messages = if !vision_chat {
            let mut prompt = String::new();
            print!("> ");
            io::stdout().flush().unwrap();
            io::stdin()
                .read_line(&mut prompt)
                .expect("Failed to get input");
            if prompt.is_empty() {
                return;
            }

            // Set the handler to terminate all seqs, so allowing cancelling running
            *CTRLC_HANDLER.lock().unwrap() = &terminate_handler;

            let mut user_message: IndexMap<String, MessageContent> = IndexMap::new();
            user_message.insert("role".to_string(), Either::Left("user".to_string()));
            user_message.insert("content".to_string(), Either::Left(prompt));
            messages.push(user_message);

            RequestMessage::Chat(messages.clone())
        } else {
            let mut prompt = String::new();
            print!("Prompt > ");
            io::stdout().flush().unwrap();
            io::stdin()
                .read_line(&mut prompt)
                .expect("Failed to get input");
            if prompt.is_empty() {
                return;
            }

            let mut image_url = String::new();
            print!("Image URL or path, [ENTER] for no image > ");
            io::stdout().flush().unwrap();
            io::stdin()
                .read_line(&mut image_url)
                .expect("Failed to get input");
            if image_url.is_empty() {
                return;
            }
            if image_url.as_str() != "\n" {
                let url_unparsed = image_url.trim();

                let image = util::parse_image_url(url_unparsed)
                    .await
                    .expect("Failed to read image from URL/path");
                images.push(image);
            }

            // Set the handler to terminate all seqs, so allowing cancelling running
            *CTRLC_HANDLER.lock().unwrap() = &terminate_handler;

            let mut user_message: IndexMap<String, MessageContent> = IndexMap::new();
            user_message.insert("role".to_string(), Either::Left("user".to_string()));
            user_message.insert("content".to_string(), Either::Left(prompt));
            messages.push(user_message);

            RequestMessage::VisionChat {
                images: images.clone(),
                messages: messages.clone(),
            }
        };

        let (tx, mut rx) = channel(10_000);
        let req = Request::Normal(NormalRequest {
            id: mistralrs.next_request_id(),
            messages: request_messages,
            sampling_params: sampling_params.clone(),
            response: tx,
            return_logprobs: false,
            is_streaming: true,
            constraint: Constraint::None,
            suffix: None,
            adapters: None,
            tool_choice: None,
            tools: None,
            logits_processors: None,
        });
        sender.send(req).await.unwrap();

        let mut assistant_output = String::new();

        let start = Instant::now();
        let mut toks = 0;
        while let Some(resp) = rx.recv().await {
            match resp {
                Response::Chunk(chunk) => {
                    let choice = &chunk.choices[0];
                    assistant_output.push_str(&choice.delta.content);
                    print!("{}", choice.delta.content);
                    toks += 3usize; // NOTE: we send toks every 3.
                    io::stdout().flush().unwrap();
                    if choice.finish_reason.is_some() {
                        if matches!(choice.finish_reason.as_ref().unwrap().as_str(), "length") {
                            print!("...");
                        }
                        break;
                    }
                }
                Response::InternalError(e) => {
                    error!("Got an internal error: {e:?}");
                    break 'outer;
                }
                Response::ModelError(e, resp) => {
                    error!("Got a model error: {e:?}, response: {resp:?}");
                    break 'outer;
                }
                Response::ValidationError(e) => {
                    error!("Got a validation error: {e:?}");
                    break 'outer;
                }
                Response::Done(_) => unreachable!(),
                Response::CompletionDone(_) => unreachable!(),
                Response::CompletionModelError(_, _) => unreachable!(),
                Response::CompletionChunk(_) => unreachable!(),
            }
        }
        if throughput {
            let time = Instant::now().duration_since(start).as_secs_f64();
            println!();
            info!("Average T/s: {}", toks as f64 / time);
        }
        let mut assistant_message: IndexMap<String, Either<String, Vec<IndexMap<String, String>>>> =
            IndexMap::new();
        assistant_message.insert("role".to_string(), Either::Left("assistant".to_string()));
        assistant_message.insert("content".to_string(), Either::Left(assistant_output));
        messages.push(assistant_message);
        println!();
    }
}
