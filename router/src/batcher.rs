/// Batching and inference logic
use crate::queue::{BatchingConfig, Entry, Queue};
use crate::{ErrorResponse, GenerateRequest};
use axum::http::StatusCode;
use axum::Json;
use std::future::Future;
use std::mem::take;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use futures::{FutureExt, TryFutureExt};
use futures::future::Map;
use nohash_hasher::IntMap;
use text_generation_client::{ClientError, Token, ShardedClient, CachedBatch, RequestsStatus, InputTokens, GenerateError};
use thiserror::Error;

use tokio::sync::oneshot;
use tokio::sync::mpsc::{channel, Sender, unbounded_channel, UnboundedReceiver};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::oneshot::Receiver;
use tokio::time::Instant;
use tokio_stream::Stream;
use tracing::{debug, info, instrument, warn, enabled, Level, error};
use crate::batch_types::BatchType;
use crate::batcher::InferError::{GenerationError, RequestQueueFull};
use crate::batcher::TokenInfos::{WithIds, WithStrings};
use crate::decoder::{Decoder, IncrementalDecoder, IncrementalDecoderWrapper};
use crate::pb::fmaas::{StopReason, TokenInfo};
use crate::pb::fmaas::StopReason::{
    Cancelled, EosToken, Error, MaxTokens, NotFinished, StopSequence, TimeLimit, TokenLimit
};
use crate::pb::fmaas::token_info::TopToken;

/// Batcher
#[derive(Clone)]
pub(crate) struct Batcher {
    /// Request queue
    sender: Sender<Vec<Entry>>,
    /// Tokenizer
    decoder: Arc<Decoder>,
}

impl Batcher {
    pub(crate) fn new<B: BatchType>(
        client: ShardedClient,
        config: BatchingConfig,
        max_waiting_tokens: usize,
        queue_size: usize,
        decoder: Decoder,
        generation_health: Arc<AtomicBool>,
        batch_type: B,
    ) -> Self {
        // Set up queue
        let (sender, receiver) = channel(queue_size);
        let decoder = Arc::new(decoder);

        // Spawn batching background task that contains all the inference logic
        tokio::spawn(std::panic::AssertUnwindSafe(batching_task(
            client,
            config.size_limit,
            max_waiting_tokens,
            Queue::new(config, batch_type, receiver),
            decoder.clone(),
            generation_health,
        )).catch_unwind().map_err(|panic| {
            error!("Batching task panicked: {panic:?}");
            std::process::exit(1);
        }));

        Self { sender, decoder }
    }

    // Returns input if queue is full
    fn enqueue_request(&self, entries: Vec<Entry>) -> Result<(), InferError> {
        self.sender.try_send(entries).map_err(|se| match se {
            TrySendError::Full(_) => RequestQueueFull(),
            TrySendError::Closed(_) => panic!["Queue closed"],
        })
    }

    /// Add a new request to the queue and return a future that will generate the text
    pub(crate) async fn infer(
        &self,
        input_length: usize,
        request: GenerateRequest,
    ) -> Result<InferResponse, InferError> {
        // One shot channel to communicate with the background batching task
        let (response_tx, response_rx) = oneshot::channel();

        // Try to add the request to the queue
        self.enqueue_request(vec![
            Entry::new(request, input_length, Some(response_tx), None),
        ])?;

        // Await on the response from the background task
        // We can safely unwrap as the background task will never drop the sender
        match response_rx.await.unwrap() {
            Ok(ir) => ir.ensure_decoded(true, &self.decoder),
            Err(err) => Err(GenerationError(err.to_string())),
        }
    }

    // Add a batch of new requests to the queue and return an vec of futures that will generate the text
    pub(crate) async fn infer_batch(
        &self,
        requests: Vec<(usize, GenerateRequest)>,
    ) -> Result<Vec<Map<Receiver<Result<InferResponse, ClientError>>,
        impl FnOnce(Result<Result<InferResponse, ClientError>, RecvError>) -> Result<InferResponse, InferError> + '_>>, InferError> {

        let mut response_chans= vec![];

        let entries: Vec<Entry> = requests.into_iter()
            .map(|(input_length, request)| {
                // One shot channel to communicate with the background batching task
                let (response_tx, response_rx) = oneshot::channel();
                response_chans.push(response_rx
                    .map(move |r: Result<Result<InferResponse, ClientError>, RecvError>| match r.unwrap() {
                        Ok(ir) => ir.ensure_decoded(true, &self.decoder),
                        Err(err) => Err(GenerationError(err.to_string())),
                    })
                );

                Entry::new(request, input_length, Some(response_tx), None)
            }).collect();

        // Try to add the request to the queue
        self.enqueue_request(entries)?;

        Ok(response_chans)
    }

    /// Add a new request to the queue and return a stream that will generate the text
    pub(crate) async fn infer_stream<T, C>(
        &self,
        input_length: usize,
        request: GenerateRequest,
        result_map: fn (Result<InferResponse, InferError>) -> T,
        on_drop: fn (&C, u32, StopReason, Option<Times>, String, Option<InferError>),
        on_drop_context: C,
    ) -> Result<ResponseStream<T, C>, InferError> {
        // One shot channel to communicate with the background batching task
        let (response_tx, response_rx) = unbounded_channel();

        // Send first response with input token count (and text if requested), and random seed used
        response_tx.send(Ok(InferResponse{
            in_token_count: input_length as u32,
            output_text: request.parameters.include_input_text
                .then(|| request.inputs.clone())
                .unwrap_or_default(),
            seed: request.parameters.seed.unwrap_or_default(),
            ..Default::default()
        })).unwrap_or_default();

        let has_stop_seq = !request.parameters.stop_seqs.is_empty();
        let include_token_info = request.parameters.include_gen_tokens;

        // Try to add the request to the queue
        self.enqueue_request(vec![
            Entry::new(request, input_length, None, Some(response_tx)),
        ])?;

        Ok(ResponseStream {
            inner: response_rx,
            map_func: result_map,
            decoder: Some(self.decoder.clone()),
            include_token_info,
            on_drop,
            on_drop_context: Arc::new(on_drop_context),
            token_count: 0,
            output: if has_stop_seq {
                // If stop sequences are requested, incremental decoding is already done in
                // the batching loop
                Accumulator::String(String::new())
            } else {
                Accumulator::Decoder(IncrementalDecoderWrapper::for_decoder(
                    &self.decoder, self.decoder.seq2seq,
                ))
            },
            times: None,
            stop_reason: NotFinished,
            err: None,
        })
    }
}

enum Accumulator {
    String(String),
    Decoder(IncrementalDecoderWrapper)
}

impl Accumulator {
    fn into_string(self) -> String {
        match self {
            Self::String(string) => string,
            Self::Decoder(idw) => idw.into_string(),
        }
    }
}

impl Default for Accumulator {
    fn default() -> Self {
        Self::String(String::new())
    }
}

/// State associated with the ongoing response stream
pub struct ResponseStream<T, C> {
    inner: UnboundedReceiver<Result<InferResponse, ClientError>>,
    map_func: fn (Result<InferResponse, InferError>) -> T,
    // This is only an option to avoid Arc clones when used in poll_next
    decoder: Option<Arc<Decoder>>,
    include_token_info: bool,
    on_drop: fn (&C, u32, StopReason, Option<Times>, String, Option<InferError>),
    on_drop_context: Arc<C>,
    token_count: u32,
    output: Accumulator,
    times: Option<Times>,
    stop_reason: StopReason,
    err: Option<InferError>,
}

impl<T, C> Drop for ResponseStream<T, C> {
    fn drop(&mut self) {
        if self.stop_reason == NotFinished {
            self.stop_reason = match self.err {
                Some(_) => Error,
                None => Cancelled,
            }
        }
        (self.on_drop)(
            &self.on_drop_context, self.token_count, self.stop_reason,
            take(&mut self.times),
            take(&mut self.output).into_string(),
            take(&mut self.err)
        );
    }
}

impl<T, C> Stream for ResponseStream<T, C> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let next = self.inner.poll_recv(cx)
                .map_err(|err| GenerationError(err.to_string()))
                .map(|o| match o {
                    Some(mut res) => {
                        let mut decode_err = None;
                        match &mut res {
                            Ok(ir) => {
                                self.token_count = ir.gen_token_count;
                                self.stop_reason = ir.reason;
                                if ir.times.is_some() {
                                    self.times = take(&mut ir.times);
                                }
                                let token = match &ir.tokens {
                                    WithIds(toks) if !toks.is_empty() => Some(&toks[0]),
                                    _ => None
                                };
                                // Detatch and reattach the decoder to appease borrow checker
                                // while avoiding having to clone Arcs
                                let decoder = take(&mut self.decoder);
                                match &mut self.output {
                                    Accumulator::String(str) => {
                                        str.push_str(&*ir.output_text);
                                    },
                                    Accumulator::Decoder(id) => {
                                        if let Some(tok) = token {
                                            match id.next(
                                                tok.token_id,
                                                decoder.as_ref().unwrap(),
                                            ) {
                                                Ok(text) => ir.output_text = text,
                                                Err(err) => decode_err = Some(err),
                                            }
                                        }
                                        // Add remainder if this is the last one
                                        if decode_err.is_none() && ir.reason != NotFinished {
                                            match id.flush(decoder.as_ref().unwrap()) {
                                                Ok(text) => ir.output_text += &text,
                                                Err(err) => decode_err = Some(err),
                                            }
                                        }
                                    }
                                }
                                self.decoder = decoder;
                                if !self.include_token_info {
                                    ir.tokens.clear();
                                }
                                ir.decode_token_infos(&self.decoder.as_ref().unwrap());
                                if ir.tokens.is_empty() && ir.output_text.is_empty()
                                    && ir.reason == NotFinished && ir.gen_token_count != 0 {
                                    // Don't include response if it's empty, unless it's the first
                                    return None
                                }
                            },
                            Err(err) => {
                                self.err = Some(err.clone());
                            }
                        }
                        if let Some(err) = decode_err {
                            self.err = Some(err.clone());
                            res = Err(err);
                        }
                        Some(Some((self.map_func)(res)))
                    },
                    None => Some(None),
                });
            if let Poll::Ready(None) = next {
                // Skip if output is empty (for example was a special token)
                continue;
            }
            return next.map(Option::unwrap);
        }
    }
}

/// Batching logic
/// Will be launched in a background Tokio task
///
/// Batches requests and sends them to the inference server
// #[instrument(skip(client, receiver, shared))]
async fn batching_task<B: BatchType>(
    mut client: ShardedClient,
    batch_size_limit: usize,
    max_waiting_tokens: usize,
    mut queue: Queue<B>,
    decoder: Arc<Decoder>,
    generation_health: Arc<AtomicBool>,
) {
    let mut processor = TokenProcessor {
        entries: IntMap::default(),
        decoder: &decoder,
        generation_health,
    };

    // Get the next batch from the queue
    // This batch might be smaller than the maximum batch size if there are not enough requests
    // waiting in the queue
    while let Some(batch) = queue.next_batch(processor.entries()).await {
        if enabled!(Level::DEBUG) {
            debug!["Pulled batch of {} request(s) from queue: {:?}", batch.requests.len(),
                batch.requests.iter().map(|r| r.id).collect::<Vec<u64>>()];
        }
        log_new_batch(batch.id, processor.entries());

        let mut cached_batch = processor.wrap_future(
            client.prefill(batch, vec![]), None,
        ).await;
        let mut waiting_tokens = 1;

        // We loop until we do not receive any cached batch from the inference server (== until
        // all requests have met their stopping criteria)
        while let Some(batch) = cached_batch {
            let batch_size = processor.entries().len();
            let batch_id = batch.batch_id;
            let some_completed = some_completed(&batch);
            let mut batches = vec![batch];

            // If the current batch is too small, we try to add more requests to it
            if should_try_to_grow_batch(
                processor.entries(),
                &queue,
                batch_size_limit,
                waiting_tokens,
                max_waiting_tokens,
                some_completed,
            ) {
                // Try to get a new batch
                if let Some(new_batch) = queue.try_next_batch(processor.entries()) {
                    info!(
                        "DEBUG: Pulled batch of {} extra request(s) from queue: {:?}",
                        new_batch.requests.len(),
                        new_batch.requests.iter().map(|r| r.id).collect::<Vec<u64>>()
                    );

                    // Determine whether existing batch needs pruning
                    let to_prune = match &batches[0].status {
                        Some(rs) if rs.completed_ids.is_empty() => vec![],
                        _ => batches.clone(),
                    };

                    // Generate one token for this new batch to have the attention past in cache
                    let first_new_id = new_batch.requests.first()
                        .expect("Batch can't be empty here").id;
                    let new_cached_batch = processor.wrap_future(
                        client.prefill(new_batch, to_prune), Some(first_new_id),
                    ).await;

                    // Hack for now - update existing batch based on pruning that would have been done
                    match batches[0].status.as_mut() {
                        Some(rs) => rs.completed_ids.clear(),
                        None => batches.clear(),
                    };

                    // Reset waiting counter
                    waiting_tokens = 1;
                    // Extend current batch with the new batch
                    if let Some(new_batch) = new_cached_batch {
                        let new_batch_id = new_batch.batch_id;
                        batches.push(new_batch);
                        let new_batch_size = processor.entries().len();
                        let added_batch_size = new_batch_size - batch_size;
                        let combined_batch_id;
                        if batch_size > 0 {
                            combined_batch_id = batch_id;
                            if added_batch_size > 0 {
                                info!("Extending batch #{} of {} with additional batch #{} of {}",
                                batch_id, batch_size, new_batch_id, added_batch_size);
                            }
                        } else {
                            combined_batch_id = new_batch_id;
                            if new_batch_size > 0 {
                                info!("Replacing completed batch #{} with new batch #{} of {}",
                                batch_id, new_batch_id, new_batch_size);
                            }
                        }
                        if added_batch_size > 0 {
                            log_new_batch(combined_batch_id, processor.entries());
                        }
                    }
                }
            }

            cached_batch = processor.wrap_future(
                client.next_token(batches), None,
            ).await;
            waiting_tokens += 1;
        }
    }
}

/// Determines whether we should attempt to pull more requests from the queue
fn should_try_to_grow_batch<B: BatchType>(
    entries: &IntMap<u64, Entry>,
    queue: &Queue<B>,
    max_batch_size: usize,
    waiting_tokens: u32,
    max_waiting_tokens: usize,
    requests_completed: bool,
) -> bool {
    waiting_tokens > 5 && entries.len() < max_batch_size && (
        waiting_tokens as usize >= max_waiting_tokens
            || requests_completed
            || queue.next_entry_waiting_too_long()
    )
}

fn log_new_batch(id: u64, entries: &IntMap<u64, Entry>) {
    let bs = entries.len();
    if bs != 0 {
        //TODO improve what's printed here
        let total_toks = entries.iter().map(|(_, e)| e.input_length).sum::<usize>();
        let max_new_toks = entries.iter().map(
            |(_, e)| e.request.parameters.max_new_tokens - e.generated_tokens
        ).max().unwrap();
        info!["New or updated batch #{} of size {} ({} total toks), max new toks = {}",
                        id, bs, total_toks, max_new_toks];
    }
}

fn some_completed(batch: &CachedBatch) -> bool {
    batch.status.as_ref().map_or(true, |s| !s.completed_ids.is_empty())
}

struct TokenProcessor<'a> {
    entries: IntMap<u64, Entry>,
    decoder: &'a Decoder,
    generation_health: Arc<AtomicBool>,
}

impl<'a> TokenProcessor<'a> {
    /// Mutably borrow the entries map
    fn entries(&mut self) -> &mut IntMap<u64, Entry> {
        &mut self.entries
    }

    /// Wrap a future inside a match statement to handle errors and send the response to the Batcher
    async fn wrap_future(
        &mut self,
        future: impl Future<Output = Result<
            Option<(Vec<Token>, Vec<InputTokens>, Vec<GenerateError>, u64)>, ClientError
        >>,
        // First request id in this batch if it doesn't comprise all current entries
        start_id: Option<u64>,
    ) -> Option<CachedBatch> {
        match future.await {
            Ok(Some((generated_tokens, input_tokens,
                        errors, next_batch_id))
            ) => {
                self.process_input_tokens(input_tokens);
                let completed_request_ids = self.process_next_tokens(
                    generated_tokens, errors,
                );
                // Update health
                self.generation_health.store(true, Ordering::SeqCst);
                Some(CachedBatch{
                    batch_id: next_batch_id,
                    status: completed_request_ids.map(|c| RequestsStatus{completed_ids: c}),
                })
            },
            Ok(None) => None,
            // If we have an error, we discard the whole batch
            Err(err) => {
                // Update health
                self.generation_health.store(false, Ordering::SeqCst);
                self.send_errors(err, start_id);
                None
            },
        }
    }

    /// Send errors to the Batcher for all `request_ids`
    fn send_errors(&mut self, error: ClientError, start_id: Option<u64>) {
        self.entries.retain(|id, entry| {
            if matches![start_id, Some(sid) if *id < sid] {
                // Keep entries that weren't in the failed request batch
                return true
            }
            // unwrap_or is valid here as we don't care if the receiver is gone.
            entry.send_final(Err(error.clone())).unwrap_or_default();
            false
        });
    }

    fn check_stopping_criteria(
        e: &Entry, last_token_id: u32, eos_token_id: u32, last_text: Option<&String>,
    ) -> StopReason {
        let params = &e.request.parameters;
        match params.deadline {
            Some(deadline) if Instant::now() > deadline => TimeLimit,
            _ if e.generated_tokens < params.min_new_tokens => NotFinished,
            _ if last_token_id == eos_token_id => EosToken,
            _ if e.generated_tokens >= params.max_new_tokens =>
                if params.max_is_token_limit { TokenLimit } else { MaxTokens }
            _ if TokenProcessor::matches_stop_sequence(e, last_text) => StopSequence,
            _ => NotFinished,
        }
    }

    fn matches_stop_sequence(e: &Entry, last_text: Option<&String>) -> bool {
        match last_text {
            Some(text) => {
                // We compare byte subslices to avoid utf8 boundary problem
                let output = e.output.as_ref().unwrap().output().as_bytes();
                let next_off = (output.len() + 1) - text.len();
                e.request.parameters.stop_seqs.iter().map(|ss| (ss.as_bytes(), ss.len())).any(
                    |(ss, len)| output[next_off.checked_sub(len).unwrap_or(0)..]
                        .windows(len).rev().any(|w| w == ss)
                )
            },
            None => false,
        }
    }

    /// Add returned input tokens to their corresponding entries
    fn process_input_tokens(&mut self, inputs: Vec<InputTokens>) {
        for input in inputs.into_iter() {
            let request_id = input.request_id;
            let e = self.entries.get_mut(&request_id)
                .expect("ID not found. This is a bug.");
            // This should be before any generated tokens are processed
            assert_eq!(e.generated_tokens, 0);

            if let Some(stream) = e.stream_tx.as_ref() {
                // In progress stream, send individual token response
                let response = InferResponse::stream_input_info(input.tokens);
                stream.send(Ok(response)).unwrap_or_default();
            } else {
                e.input_tokens = input.tokens;
            }
        }
    }

    /// Store next token for each sequence, evaluate stopping criteria,
    /// send output back for streaming or completed requests
    fn process_next_tokens(
        &mut self, outputs: Vec<Token>, errors: Vec<GenerateError>,
    ) -> Option<Vec<u64>> {
        let mut completed_ids = vec![];
        let request_count = outputs.len();
        for output in outputs.into_iter() {
            let request_id = output.request_id;
            let next_token_id = output.token_id;

            let e = self.entries.get_mut(&request_id)
                .expect("ID not found. This is a bug.");

            if e.generated_tokens == 0 && !e.request.parameters.stop_seqs.is_empty() {
                e.output = Some(IncrementalDecoderWrapper::for_decoder(
                    &self.decoder, self.decoder.seq2seq,
                ));
            }

            e.generated_tokens += 1;
            let is_stream = e.stream_tx.is_some();
            let token = match is_stream {
                true => Some(output),
                false => {
                    // Only accumulate token vecs in the entry if this is a non-streaming request
                    // (otherwise they're sent immediately)
                    e.token_ids.push(next_token_id);
                    if e.request.parameters.include_gen_tokens {
                        e.tokens.push(output);
                    }
                    None
                }
            };

            let mut text = None;
            if let Some(idecoder) = &mut e.output {
                // We only do the token decoding at this stage if stop_sequence(s) are provided,
                // otherwise it can be deferred to run in per-response tasks rather than
                // the main batching loop
                match idecoder.next(next_token_id, self.decoder) {
                    Ok(decoded) => {
                        text = Some(decoded);
                    },
                    Err(err) => {
                        // Decoding error, abort the request
                        e.send_final(Err(ClientError::Generation(err.to_string())))
                            .unwrap_or_default();
                        self.entries.remove(&request_id).unwrap();
                        info!("DEBUG: Completed req id {request_id} with reason {Error:?}");
                        completed_ids.push(request_id);
                        continue
                    },
                }
            }

            // Evaluate stopping criteria
            let mut stop_reason = TokenProcessor::check_stopping_criteria(
                e, next_token_id, self.decoder.eos_token_id, text.as_ref()
            );

            if stop_reason != NotFinished {
                // Stop criteria met, send final response for both streaming and unary cases
                let mut e = self.entries.remove(&request_id).unwrap();
                // Flush the output if we are doing incremental decoding
                let mut decode_err = None;
                if let Some(t) = text.as_mut() {
                    if let Err(err) = e.output.as_mut().unwrap()
                        .flush(self.decoder).map(|s| t.push_str(&s)) {
                        decode_err = Some(err);
                    }
                }
                let response = match decode_err {
                    Some(err) => Err(ClientError::Generation(err.to_string())),
                    _ if is_stream => Ok(InferResponse::stream_final(
                        token.unwrap(), text, &e, stop_reason
                    )),
                    _ => Ok(InferResponse::unary(&mut e, self.decoder.seq2seq, stop_reason)),
                };
                // unwrap_or is valid here as we don't care if the receiver is gone.
                e.send_final(response).unwrap_or_default();

            } else if is_stream {
                // In progress stream, send individual token response
                let response = InferResponse::stream_inprog(
                    token.unwrap(), e.generated_tokens, text
                );
                if e.stream_tx.as_ref().unwrap().send(Ok(response)).is_err() {
                    // If receiver closed (request cancelled), cancel this entry
                    self.entries.remove(&request_id).unwrap();
                    stop_reason = Cancelled;
                    //TODO include request context
                    warn!("Aborted in-progress generation for streaming request {request_id} cancelled by client");
                }
            }

            // Only check non-streaming response channel every 16 tokens to avoid repeated atomic access
            else if e.generated_tokens % 16 == 0 && e.response_tx.as_ref().unwrap().is_closed() {
                // If receiver closed (request cancelled), cancel this entry
                self.entries.remove(&request_id).unwrap();
                stop_reason = Cancelled;
                //TODO include request context
                warn!("Aborted in-progress generation for request {request_id} cancelled by client");
            }

            if stop_reason != NotFinished {
                debug!("Completed req id {request_id} with reason {stop_reason:?}");
                completed_ids.push(request_id);
            }
        }

        // Process any errors
        for error in errors.into_iter() {
            let request_id = error.request_id;

            let e = self.entries.get_mut(&request_id)
                .expect("ID not found. This is a bug.");

                // Abort the request
                // TODO maybe send Ok result with Error stop reason instead,
                // so that any tokens already generated will be included in unary case
                let message = match e.generated_tokens {
                    0 => error.message.clone(),
                    n => format!["Error after generating {} tokens: {}", n, error.message],
                };
                e.send_final(Err(ClientError::Generation(message))).unwrap_or_default();
                self.entries.remove(&request_id).unwrap();
                info!("DEBUG: Completed req id {request_id} with reason {Error:?}: {}", error.message);
                completed_ids.push(request_id);
        }

        // Return None if all requests in this batch have completed, otherwise the list of completed ids
        if completed_ids.len() == request_count { None } else { Some(completed_ids) }
    }
}

#[derive(Debug)]
pub(crate) struct Times {
    // Queue start time
    pub(crate) queued: Instant,
    // Generation start time
    pub(crate) start: Instant,
    // Generation end time
    pub(crate) end: Instant,
}

impl From<&Entry> for Times {
    fn from(entry: &Entry) -> Self {
        Self{
            queued: entry.queue_time, start: entry.batch_time.unwrap(), end: Instant::now(),
        }
    }
}

/// This enum initially contains a vec of Token structs
/// received from the shards and containing token ids.
/// It is decoded to a vec of TokenInfo structs containing
/// the token strings, which is sent in the external gRPC response.
#[derive(Debug)]
pub(crate) enum TokenInfos {
    WithIds(Vec<Token>),
    WithStrings(Vec<TokenInfo>)
}

impl Default for TokenInfos {
    fn default() -> Self {
        WithIds(vec![])
    }
}

impl TokenInfos {
    fn clear(&mut self) {
        match self {
            WithStrings(tis) => tis.clear(),
            WithIds(tis) => tis.clear(),
        }
    }
    fn is_empty(&self) -> bool {
        match self {
            WithStrings(tis) => tis.is_empty(),
            WithIds(tis) => tis.is_empty(),
        }
    }
    pub(crate) fn to_final_vec(self) -> Vec<TokenInfo> {
        match self {
            WithStrings(tis) => tis,
            _ => vec![],
        }
    }
    fn decode(&mut self, decoder: &Decoder) {
        if let WithIds(toks) = &self {
            *self = WithStrings(toks.iter()
                .map(|t| TokenInfos::decode_token_info(t, decoder))
                .collect());
        }
    }
    fn decode_token_info(with_ids: &Token, decoder: &Decoder) -> TokenInfo {
        TokenInfo{
            text: decoder.id_to_token(with_ids.token_id),
            logprob: with_ids.logprob,
            rank: with_ids.rank,
            top_tokens: with_ids.top_tokens.iter().map(|tt| TopToken{
                text: decoder.id_to_token(tt.token_id),
                logprob: tt.logprob,
            }).collect(),
        }
    }
}


#[derive(Debug, Default)]
pub(crate) struct InferResponse {
    pub(crate) output_text: String,
    /// whether or not the token ids have been decoded yet
    pub(crate) is_decoded: bool,
    /// Total generated tokens so far
    pub(crate) gen_token_count: u32,
    // Set/used only for unary responses
    pub(crate) token_ids: Vec<u32>,
    // This will be max length 1 in streaming case
    // Only set in unary case if extra token info is requested
    pub(crate) tokens: TokenInfos,
    pub(crate) in_tokens: TokenInfos,
    pub(crate) reason: StopReason,
    pub(crate) in_token_count: u32,
    pub(crate) times: Option<Times>,
    /// Random seed used, only applicable to sampling
    pub(crate) seed: u64,
}

impl InferResponse {
    /// A dedicated message is sent with the input token info, if requested
    fn stream_input_info(in_tokens: Vec<Token>) -> Self {
        Self {
            in_token_count: in_tokens.len() as u32,
            in_tokens: WithIds(in_tokens),
            is_decoded: true,
            ..Default::default()
        }
    }
    /// Response message for in-progress stream
    fn stream_inprog(token: Token, count: u32, text: Option<String>) -> Self {
        Self {
            is_decoded: text.is_some(),
            output_text: text.unwrap_or_default(),
            gen_token_count: count,
            tokens: WithIds(vec![token]),
            ..Default::default()
        }
    }
    /// Final stream response message
    fn stream_final(
        token: Token, text: Option<String>, entry: &Entry, stop_reason: StopReason
    ) -> Self {
        Self {
            is_decoded: text.is_some(),
            output_text: text.unwrap_or_default(),
            gen_token_count: entry.generated_tokens,
            tokens: WithIds(vec![token]),
            reason: stop_reason,
            times: Some(entry.into()),
            seed: entry.request.parameters.seed.unwrap_or_default(),
            ..Default::default()
        }
    }
    /// Unary response message
    fn unary(entry: &mut Entry, seq2seq: bool, stop_reason: StopReason) -> Self {
        let mut text = String::new();
        if entry.request.parameters.include_input_text {
            text += &*entry.request.inputs;
            if seq2seq {
                text += "\n\n";
            }
        }
        let is_decoded;
        if let Some(out_decoder) = take(&mut entry.output) {
            is_decoded = true;
            if text.is_empty() {
                text = out_decoder.into_string();
            } else {
                text.push_str(out_decoder.output())
            }
        } else {
            is_decoded = false;
        }
        Self {
            output_text: text,
            is_decoded,
            gen_token_count: entry.generated_tokens,
            token_ids: take(&mut entry.token_ids),
            tokens: WithIds(take(&mut entry.tokens)),
            in_tokens: WithIds(take(&mut entry.input_tokens)),
            reason: stop_reason,
            times: Some((&*entry).into()),
            in_token_count: entry.input_length as u32,
            seed: entry.request.parameters.seed.unwrap_or_default(),
        }
    }
    /// If time limit is expired before generation starts
    pub(crate) fn early_timeout(entry: &Entry) -> Self {
        Self {
            reason: TimeLimit,
            is_decoded: true,
            // We only include input token count in the unary case, since it will have
            // already been sent in the streaming case
            in_token_count: if entry.response_tx.is_some() { entry.input_length as u32 } else { 0 },
            times: Some((&*entry).into()),
            ..Default::default()
        }
    }

    pub(crate) fn decode_output_text(
        &mut self, decoder: &Decoder, first: bool
    ) -> Result<(), InferError> {
        if !self.is_decoded {
            self.output_text += &*decoder.decode(
                take(&mut self.token_ids), first, true,
            )?;
            self.is_decoded = true;
        }
        Ok(())
    }

    pub(crate) fn decode_token_infos(&mut self, decoder: &Decoder) {
        self.tokens.decode(decoder);
        self.in_tokens.decode(decoder);
    }

    pub(crate) fn ensure_decoded(
        mut self, first: bool, decoder: &Decoder
    ) -> Result<InferResponse, InferError> {
        self.decode_token_infos(decoder);
        self.decode_output_text(decoder, first).map(|_| self)
    }
}

#[derive(Debug, Error, Clone)]
pub enum InferError {
    #[error("Request failed during generation: {0}")]
    GenerationError(String),
    #[error("Request failed during detokenization: {0}")]
    DetokenizationError(String),
    #[error("Server too busy")]
    RequestQueueFull(),
}

/// Convert to Axum supported format
impl From<InferError> for (StatusCode, Json<ErrorResponse>) {
    fn from(err: InferError) -> Self {
        match err {
            _ => (
                StatusCode::FAILED_DEPENDENCY,
                Json(ErrorResponse {
                    error: err.to_string(),
                }),
            ),
        }
    }
}