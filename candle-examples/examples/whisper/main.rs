#![allow(dead_code)]
// https://github.com/openai/whisper/blob/main/whisper/model.py
// TODO:
// - kv-cache support?
// - Language detection?
// - Batch size greater than 1.
// - More token filters (SuppressBlanks, ApplyTimestampRules).

#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

use anyhow::{Error as E, Result};
use candle::{safetensors::Load, DType, Device, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use clap::Parser;
use hf_hub::{api::sync::Api, Repo, RepoType};
use rand::{distributions::Distribution, SeedableRng};
use tokenizers::Tokenizer;

mod audio;
mod model;
use model::{Config, Whisper};

const DTYPE: DType = DType::F32;

// Audio parameters.
const SAMPLE_RATE: usize = 16000;
const N_FFT: usize = 400;
const N_MELS: usize = 80;
const HOP_LENGTH: usize = 160;
const CHUNK_LENGTH: usize = 30;
const N_SAMPLES: usize = CHUNK_LENGTH * SAMPLE_RATE; // 480000 samples in a 30-second chunk
const N_FRAMES: usize = N_SAMPLES / HOP_LENGTH; // 3000 frames in a mel spectrogram input
const N_SAMPLES_PER_TOKEN: usize = HOP_LENGTH * 2; // the initial convolutions has stride 2
const FRAMES_PER_SECOND: usize = SAMPLE_RATE / HOP_LENGTH; // 10ms per audio frame
const TOKENS_PER_SECOND: usize = SAMPLE_RATE / N_SAMPLES_PER_TOKEN; // 20ms per audio token

const NO_SPEECH_THRESHOLD: f64 = 0.6;
const LOGPROB_THRESHOLD: f64 = -1.0;
const TEMPERATURES: [f64; 6] = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
const COMPRESSION_RATIO_THRESHOLD: f64 = 2.4;

// Tokenizer dependent bits.
const SOT_TOKEN: u32 = 50257;
const EOT_TOKEN: u32 = 50256;
const NO_SPEECH_TOKEN: u32 = 50361;
const NO_TIMESTAMP_TOKEN: u32 = 50362;
// From the _get_suppress_tokens function + 50362 (no timestamp)
// https://github.com/openai/whisper/blob/f572f2161ba831bae131364c3bffdead7af6d210/whisper/decoding.py#L605
const SUPPRESS_TOKENS: [u32; 91] = [
    1, 2, 7, 8, 9, 10, 14, 25, 26, 27, 28, 29, 31, 58, 59, 60, 61, 62, 63, 90, 91, 92, 93, 357,
    366, 438, 532, 685, 705, 796, 930, 1058, 1220, 1267, 1279, 1303, 1343, 1377, 1391, 1635, 1782,
    1875, 2162, 2361, 2488, 3467, 4008, 4211, 4600, 4808, 5299, 5855, 6329, 7203, 9609, 9959,
    10563, 10786, 11420, 11709, 11907, 13163, 13697, 13700, 14808, 15306, 16410, 16791, 17992,
    19203, 19510, 20724, 22305, 22935, 27007, 30109, 30420, 33409, 34949, 40283, 40493, 40549,
    47282, 49146, 50257, 50357, 50358, 50359, 50360, 50361, 50362,
];

#[derive(Debug, Clone)]
struct DecodingResult {
    tokens: Vec<u32>,
    text: String,
    avg_logprob: f64,
    no_speech_prob: f64,
    temperature: f64,
    compression_ratio: f64,
}

#[derive(Debug, Clone)]
struct Segment {
    start: f64,
    duration: f64,
    dr: DecodingResult,
}

struct Decoder {
    model: Whisper,
    rng: rand::rngs::StdRng,
    tokenizer: Tokenizer,
    suppress_tokens: Tensor,
}

impl Decoder {
    fn new(model: Whisper, tokenizer: Tokenizer, seed: u64, device: &Device) -> Result<Self> {
        let suppress_tokens: Vec<f32> = (0..model.config.vocab_size as u32)
            .map(|i| {
                if SUPPRESS_TOKENS.contains(&i) {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect();
        let suppress_tokens = Tensor::new(suppress_tokens.as_slice(), device)?;
        Ok(Self {
            model,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
            tokenizer,
            suppress_tokens,
        })
    }

    fn decode(&mut self, mel: &Tensor, t: f64) -> Result<DecodingResult> {
        let model = &self.model;
        let audio_features = model.encoder.forward(mel)?;
        println!("audio features: {:?}", audio_features.dims());
        let sample_len = model.config.max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![SOT_TOKEN];
        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), mel.device())?;

            // The model expects a batch dim but this inference loop does not handle
            // it so we add it at this point.
            let tokens_t = tokens_t.unsqueeze(0)?;
            let logits = model.decoder.forward(&tokens_t, &audio_features)?;
            let logits = logits.squeeze(0)?;

            // Extract the no speech probability on the first iteration by looking at the first
            // token logits and the probability for the according token.
            if i == 0 {
                no_speech_prob = softmax(&logits.get(0)?, 0)?
                    .get(NO_SPEECH_TOKEN as usize)?
                    .to_scalar::<f32>()? as f64;
            }

            let (seq_len, _) = logits.dims2()?;
            let logits = logits
                .get(seq_len - 1)?
                .broadcast_add(&self.suppress_tokens)?;
            let next_token = if t > 0f64 {
                let prs = softmax(&(&logits / t)?, 0)?;
                let logits_v: Vec<f32> = prs.to_vec1()?;
                let distr = rand::distributions::WeightedIndex::new(&logits_v)?;
                distr.sample(&mut self.rng) as u32
            } else {
                let logits_v: Vec<f32> = logits.to_vec1()?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            tokens.push(next_token);
            let prob = softmax(&logits, candle::D::Minus1)?
                .get(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            if next_token == EOT_TOKEN || tokens.len() > model.config.max_target_positions {
                break;
            }
            sum_logprob += prob.ln();
        }
        let text = self
            .tokenizer
            .decode(tokens.clone(), true)
            .map_err(E::msg)?;
        let avg_logprob = sum_logprob / tokens.len() as f64;

        Ok(DecodingResult {
            tokens,
            text,
            avg_logprob,
            no_speech_prob,
            temperature: t,
            compression_ratio: f64::NAN,
        })
    }

    fn decode_with_fallback(&mut self, segment: &Tensor) -> Result<DecodingResult> {
        for (i, &t) in TEMPERATURES.iter().enumerate() {
            let dr: Result<DecodingResult> = self.decode(segment, t);
            if i == TEMPERATURES.len() - 1 {
                return dr;
            }
            // On errors, we try again with a different temperature.
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio > COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < LOGPROB_THRESHOLD;
                    if !needs_fallback || dr.no_speech_prob > NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    println!("Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    fn run(&mut self, mel: &Tensor) -> Result<Vec<Segment>> {
        let (_, _, content_frames) = mel.dims3()?;
        let mut seek = 0;
        let mut segments = vec![];
        while seek < content_frames {
            let start = std::time::Instant::now();
            let time_offset = (seek * HOP_LENGTH) as f64 / SAMPLE_RATE as f64;
            let segment_size = usize::min(content_frames - seek, N_FRAMES);
            let mel_segment = mel.narrow(2, seek, segment_size)?;
            let segment_duration = (segment_size * HOP_LENGTH) as f64 / SAMPLE_RATE as f64;
            let dr = self.decode_with_fallback(&mel_segment)?;
            seek += segment_size;
            if dr.no_speech_prob > NO_SPEECH_THRESHOLD && dr.avg_logprob < LOGPROB_THRESHOLD {
                println!("no speech detected, skipping {seek} {dr:?}");
                continue;
            }
            let segment = Segment {
                start: time_offset,
                duration: segment_duration,
                dr,
            };
            println!("{seek}: {segment:?}, in {:?}", start.elapsed());
            segments.push(segment)
        }
        Ok(segments)
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run on CPU rather than on GPU.
    #[arg(long)]
    cpu: bool,

    #[arg(long)]
    model_id: Option<String>,

    /// The model to use, check out available models:
    /// https://huggingface.co/models?search=whisper
    #[arg(long)]
    revision: Option<String>,

    /// The input to be processed, in wav format, will default to `jfk.wav`. Alternatively
    /// this can be set to sample:jfk, sample:gb1, ... to fetch a sample from the following
    /// repo: https://huggingface.co/datasets/Narsil/candle_demo/
    #[arg(long)]
    input: Option<String>,

    /// The seed to use when generating random samples.
    #[arg(long, default_value_t = 299792458)]
    seed: u64,

    /// The mel filters in safetensors format.
    #[arg(
        long,
        default_value = "candle-examples/examples/whisper/mel_filters.safetensors"
    )]
    filters: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let device = candle_examples::device(args.cpu)?;
    let default_model = "openai/whisper-tiny.en".to_string();
    let path = std::path::PathBuf::from(default_model.clone());
    let default_revision = "refs/pr/15".to_string();
    let (model_id, revision) = match (args.model_id, args.revision) {
        (Some(model_id), Some(revision)) => (model_id, revision),
        (Some(model_id), None) => (model_id, "main".to_string()),
        (None, Some(revision)) => (default_model, revision),
        (None, None) => (default_model, default_revision),
    };

    let (config_filename, tokenizer_filename, weights_filename, input) = if path.exists() {
        let mut config_filename = path.clone();
        config_filename.push("config.json");
        let mut tokenizer_filename = path.clone();
        tokenizer_filename.push("tokenizer.json");
        let mut model_filename = path;
        model_filename.push("model.safetensors");
        (
            config_filename,
            tokenizer_filename,
            model_filename,
            std::path::PathBuf::from(args.input.expect("You didn't specify a file to read from yet, are using a local model, please add `--input example.wav` to read some audio file")),
        )
    } else {
        let api = Api::new()?;
        let dataset = api.dataset("Narsil/candle-examples".to_string());
        let repo = api.repo(Repo::with_revision(model_id, RepoType::Model, revision));
        let sample = if let Some(input) = args.input {
            if let Some(sample) = input.strip_prefix("sample:") {
                dataset.get(&format!("samples_{sample}.wav"))?
            } else {
                std::path::PathBuf::from(input)
            }
        } else {
            println!("No audio file submitted: Downloading https://huggingface.co/datasets/Narsil/candle_demo/blob/main/samples_jfk.wav");
            dataset.get("samples_jfk.wav")?
        };
        (
            repo.get("config.json")?,
            repo.get("tokenizer.json")?,
            repo.get("model.safetensors")?,
            sample,
        )
    };
    let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;

    let mel_filters = unsafe { candle::safetensors::MmapedFile::new(args.filters)? };
    let mel_filters = mel_filters.deserialize()?;
    let mel_filters = mel_filters.tensor("mel_80")?.load(&device)?;
    println!("loaded mel filters {:?}", mel_filters.shape());
    let mel_filters = mel_filters.flatten_all()?.to_vec1::<f32>()?;

    let mut input = std::fs::File::open(input)?;
    let (header, data) = wav::read(&mut input)?;
    println!("loaded wav data: {header:?}");
    if header.sampling_rate != SAMPLE_RATE as u32 {
        anyhow::bail!("wav file must have a {} sampling rate", SAMPLE_RATE)
    }
    let data = data.as_sixteen().expect("expected 16 bit wav file");
    let pcm_data: Vec<_> = data[..data.len() / header.channel_count as usize]
        .iter()
        .map(|v| *v as f32 / 32768.)
        .collect();
    println!("pcm data loaded {}", pcm_data.len());
    let mel = audio::pcm_to_mel(&pcm_data, &mel_filters)?;
    let mel_len = mel.len();
    let mel = Tensor::from_vec(mel, (1, N_MELS, mel_len / N_MELS), &device)?;
    println!("loaded mel: {:?}", mel.dims());

    let weights = unsafe { candle::safetensors::MmapedFile::new(weights_filename)? };
    let weights = weights.deserialize()?;
    let vb = VarBuilder::from_safetensors(vec![weights], DTYPE, &device);
    let config: Config = serde_json::from_str(&std::fs::read_to_string(config_filename)?)?;
    let model = Whisper::load(&vb, config)?;
    let mut dc = Decoder::new(model, tokenizer, args.seed, &device)?;
    dc.run(&mel)?;
    Ok(())
}
