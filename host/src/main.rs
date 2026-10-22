use anyhow::{bail, Context, Result};
use phone_protocol::{
    encode, Decoder, Frame, ANSWER, AUDIO, DIAL, HANGUP, MAX_WIRE, MODEM, PLAY, QUERY,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const RATE: u32 = 8_000;
const FRAME_SAMPLES: usize = 160;
type History = Arc<Mutex<HashMap<String, Vec<(String, String)>>>>;

#[derive(Clone, Deserialize)]
struct Config {
    serial_port: String,
    #[serde(default = "default_baud")]
    baud: u32,
    #[serde(default)]
    allow_all: bool,
    #[serde(default)]
    allowed_numbers: Vec<String>,
    whisper_cli: PathBuf,
    whisper_model: PathBuf,
    piper: PathBuf,
    piper_model: PathBuf,
    codex: PathBuf,
    workdir: PathBuf,
    #[serde(default = "default_callback_after")]
    callback_after_seconds: u64,
}
fn default_baud() -> u32 {
    460_800
}
fn default_callback_after() -> u64 {
    20
}

fn normalized_number(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let prefix = if chars.clone().next() == Some('+') {
        chars.next();
        "+"
    } else {
        ""
    };
    let digits: String = chars.filter(|c| c.is_ascii_digit()).collect();
    if (7..=15).contains(&digits.len()) {
        Some(format!("{prefix}{digits}"))
    } else {
        None
    }
}
fn is_allowed(cfg: &Config, number: &str) -> bool {
    cfg.allow_all
        || normalized_number(number).is_some_and(|n| {
            cfg.allowed_numbers
                .iter()
                .any(|x| normalized_number(x).as_deref() == Some(&n))
        })
}
fn clip_number(line: &str) -> Option<String> {
    line.strip_prefix("+CLIP:")?
        .trim()
        .strip_prefix('"')?
        .split('"')
        .next()
        .and_then(normalized_number)
}
fn call_active(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("+CLCC:") else {
        return false;
    };
    let fields: Vec<_> = rest.split(',').map(str::trim).collect();
    fields.len() >= 3 && fields[2] == "0"
}
fn send(port: &mut dyn serialport::SerialPort, kind: u8, payload: &[u8]) -> Result<()> {
    let frame = Frame::new(kind, payload).context("payload too long")?;
    let mut wire = [0; MAX_WIRE];
    let n = encode(&frame, &mut wire);
    port.write_all(&wire[..n])?;
    Ok(())
}

enum State {
    Idle,
    Ringing {
        since: Instant,
        number: Option<String>,
    },
    Listening {
        number: String,
        samples: Vec<u8>,
        voiced: bool,
        last_voice: Instant,
    },
    Thinking {
        number: String,
        since: Instant,
        job: Receiver<Result<String>>,
        callback: bool,
    },
    Dialing {
        number: String,
        answer: String,
        since: Instant,
        last_query: Instant,
    },
}

fn write_input_wav(path: &Path, samples: &[u8]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(path, spec)?;
    for &x in samples {
        wav.write_sample(((x as i16) - 128) * 256)?;
    }
    wav.finalize()?;
    Ok(())
}
fn transcribe(cfg: &Config, samples: &[u8]) -> Result<String> {
    let stem = Path::new("runtime/utterance");
    let wav = stem.with_extension("wav");
    write_input_wav(&wav, samples)?;
    let output = Command::new(&cfg.whisper_cli)
        .arg("-m")
        .arg(&cfg.whisper_model)
        .arg("-f")
        .arg(&wav)
        .arg("-l")
        .arg("ru")
        .arg("-otxt")
        .arg("-of")
        .arg(stem)
        .arg("-nt")
        .output()
        .context("start whisper-cli")?;
    if !output.status.success() {
        bail!("whisper-cli: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(fs::read_to_string(stem.with_extension("txt"))?
        .trim()
        .to_owned())
}
fn codex_answer(cfg: &Config, question: &str, previous: &[(String, String)]) -> Result<String> {
    let mut prompt = String::from("Ты отвечаешь человеку по телефону на русском языке. Веди короткий голосовой диалог. Не выполняй команды и не меняй файлы. Отвечай только текстом, без Markdown.\n");
    for (user, assistant) in previous.iter().rev().take(8).rev() {
        prompt.push_str("Пользователь: ");
        prompt.push_str(user);
        prompt.push_str("\nCodex: ");
        prompt.push_str(assistant);
        prompt.push('\n');
    }
    prompt.push_str("Пользователь: ");
    prompt.push_str(question);
    let out = Path::new("runtime/codex.txt");
    let result = Command::new(&cfg.codex)
        .args([
            "exec",
            "--skip-git-repo-check",
            "--ephemeral",
            "-s",
            "read-only",
            "-C",
        ])
        .arg(&cfg.workdir)
        .arg("-o")
        .arg(out)
        .arg(prompt)
        .output()
        .context("start codex exec")?;
    if !result.status.success() {
        bail!("codex exec: {}", String::from_utf8_lossy(&result.stderr));
    }
    Ok(fs::read_to_string(out)?.trim().to_owned())
}
fn run_turn(cfg: &Config, samples: &[u8], number: &str, history: &History) -> Result<String> {
    let question = transcribe(cfg, samples)?;
    if question.is_empty() {
        bail!("speech not recognized");
    }
    let previous = history
        .lock()
        .unwrap()
        .get(number)
        .cloned()
        .unwrap_or_default();
    let answer = codex_answer(cfg, &question, &previous)?;
    let mut guard = history.lock().unwrap();
    let entries = guard.entry(number.to_owned()).or_default();
    entries.push((question, answer.clone()));
    if entries.len() > 8 {
        entries.remove(0);
    }
    Ok(answer)
}
fn synthesize(cfg: &Config, text: &str) -> Result<Vec<u8>> {
    let path = Path::new("runtime/speech.wav");
    let mut child = Command::new(&cfg.piper)
        .arg("--model")
        .arg(&cfg.piper_model)
        .arg("--output_file")
        .arg(path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Piper")?;
    child
        .stdin
        .take()
        .context("Piper stdin")?
        .write_all(text.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("Piper: {}", String::from_utf8_lossy(&output.stderr));
    }
    let mut wav = hound::WavReader::open(path)?;
    let spec = wav.spec();
    if spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_rate == 0
        || spec.sample_format != hound::SampleFormat::Int
    {
        bail!("Piper must produce mono 16-bit PCM WAV");
    }
    let src: Vec<i16> = wav
        .samples::<i16>()
        .collect::<std::result::Result<_, _>>()?;
    if src.is_empty() {
        bail!("empty speech output");
    }
    let count = (src.len() as u64 * RATE as u64 / spec.sample_rate as u64) as usize;
    Ok((0..count)
        .map(|i| {
            let pos = i as f64 * spec.sample_rate as f64 / RATE as f64;
            let low = (pos.floor() as usize).min(src.len() - 1);
            let high = (low + 1).min(src.len() - 1);
            let f = pos - low as f64;
            let value = src[low] as f64 * (1.0 - f) + src[high] as f64 * f;
            (value / 256.0 + 128.0).round().clamp(0.0, 255.0) as u8
        })
        .collect())
}
fn speak(port: &mut dyn serialport::SerialPort, cfg: &Config, text: &str) -> Result<()> {
    let audio = synthesize(cfg, text)?;
    for chunk in audio.chunks(FRAME_SAMPLES) {
        let mut frame = [128u8; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        send(port, PLAY, &frame)?;
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn main() -> Result<()> {
    let cfg: Config =
        toml::from_str(&fs::read_to_string("phone.toml").context("read phone.toml")?)?;
    fs::create_dir_all("runtime")?;
    let history: History = Arc::new(Mutex::new(HashMap::new()));
    let mut port = serialport::new(&cfg.serial_port, cfg.baud)
        .timeout(Duration::from_millis(10))
        .open()
        .context("open ESP32 serial port")?;
    let mut decoder = Decoder::default();
    let mut state = State::Idle;
    let mut input = [0u8; 512];
    loop {
        match port.read(&mut input) {
            Ok(n) => {
                for &b in &input[..n] {
                    if let Some(frame) = decoder.push(b) {
                        match frame.kind {
                            MODEM => {
                                let line = String::from_utf8_lossy(frame.payload());
                                println!("SIM800L: {line}");
                                if line == "RING" && matches!(state, State::Idle) {
                                    state = State::Ringing {
                                        since: Instant::now(),
                                        number: None,
                                    };
                                }
                                if let Some(number) = clip_number(&line) {
                                    if let State::Ringing { number: slot, .. } = &mut state {
                                        *slot = Some(number);
                                    }
                                }
                                if line == "NO CARRIER" || line == "BUSY" {
                                    if let State::Thinking { callback, .. } = &mut state {
                                        *callback = true;
                                    } else {
                                        state = State::Idle;
                                    }
                                }
                                if call_active(&line) {
                                    if let State::Dialing { number, answer, .. } = &state {
                                        let number = number.clone();
                                        let answer = answer.clone();
                                        println!("Callback answered by {number}");
                                        speak(&mut *port, &cfg, &answer)?;
                                        send(&mut *port, HANGUP, &[])?;
                                        state = State::Idle;
                                    }
                                }
                            }
                            AUDIO => {
                                if let State::Listening {
                                    samples,
                                    voiced,
                                    last_voice,
                                    ..
                                } = &mut state
                                {
                                    let level = frame
                                        .payload()
                                        .iter()
                                        .map(|x| x.abs_diff(128) as u32)
                                        .sum::<u32>()
                                        / frame.len.max(1) as u32;
                                    if level > 8 {
                                        *voiced = true;
                                        *last_voice = Instant::now();
                                    }
                                    if *voiced {
                                        samples.extend_from_slice(frame.payload());
                                    }
                                    if samples.len() > 8_000 * 30 {
                                        samples.drain(..samples.len() - 8_000 * 30);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
        state = match state {
            State::Ringing { since, number }
                if number.is_some() || since.elapsed() > Duration::from_secs(3) =>
            {
                if let Some(number) = number.filter(|n| is_allowed(&cfg, n)) {
                    send(&mut *port, ANSWER, &[])?;
                    State::Listening {
                        number,
                        samples: Vec::new(),
                        voiced: false,
                        last_voice: Instant::now(),
                    }
                } else if cfg.allow_all {
                    send(&mut *port, ANSWER, &[])?;
                    State::Listening {
                        number: String::new(),
                        samples: Vec::new(),
                        voiced: false,
                        last_voice: Instant::now(),
                    }
                } else {
                    send(&mut *port, HANGUP, &[])?;
                    State::Idle
                }
            }
            State::Listening {
                number,
                samples,
                voiced,
                last_voice,
            } if voiced && last_voice.elapsed() > Duration::from_millis(900) => {
                let (tx, rx) = mpsc::channel();
                let job_cfg = cfg.clone();
                let key = number.clone();
                let job_history = history.clone();
                thread::spawn(move || {
                    let _ = tx.send(run_turn(&job_cfg, &samples, &key, &job_history));
                });
                State::Thinking {
                    number,
                    since: Instant::now(),
                    job: rx,
                    callback: false,
                }
            }
            State::Thinking {
                number,
                since,
                job,
                mut callback,
            } => match job.try_recv() {
                Ok(result) => {
                    let answer =
                        result.unwrap_or_else(|e| format!("Извините, возникла ошибка: {e}"));
                    if callback {
                        if number.is_empty() {
                            State::Idle
                        } else {
                            send(&mut *port, DIAL, number.as_bytes())?;
                            State::Dialing {
                                number,
                                answer,
                                since: Instant::now(),
                                last_query: Instant::now(),
                            }
                        }
                    } else {
                        speak(&mut *port, &cfg, &answer)?;
                        State::Listening {
                            number,
                            samples: Vec::new(),
                            voiced: false,
                            last_voice: Instant::now(),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if !callback
                        && since.elapsed() > Duration::from_secs(cfg.callback_after_seconds)
                        && !number.is_empty()
                    {
                        speak(
                            &mut *port,
                            &cfg,
                            "Мне нужно время подумать. Я перезвоню с ответом.",
                        )?;
                        send(&mut *port, HANGUP, &[])?;
                        callback = true;
                    }
                    State::Thinking {
                        number,
                        since,
                        job,
                        callback,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => State::Idle,
            },
            State::Dialing {
                number,
                answer,
                since,
                mut last_query,
            } => {
                if since.elapsed() > Duration::from_secs(45) {
                    send(&mut *port, HANGUP, &[])?;
                    State::Idle
                } else {
                    if last_query.elapsed() > Duration::from_secs(2) {
                        send(&mut *port, QUERY, &[])?;
                        last_query = Instant::now();
                    }
                    State::Dialing {
                        number,
                        answer,
                        since,
                        last_query,
                    }
                }
            }
            other => other,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../../phone.toml.example")).unwrap();
        assert_eq!(cfg.baud, 460800);
        assert!(!cfg.allow_all);
    }
    #[test]
    fn caller_policy() {
        let mut cfg: Config = toml::from_str(include_str!("../../phone.toml.example")).unwrap();
        assert!(is_allowed(&cfg, "+7 999 123-45-67"));
        assert!(!is_allowed(&cfg, "+79990000000"));
        cfg.allow_all = true;
        assert!(is_allowed(&cfg, "+79990000000"));
    }
    #[test]
    fn modem_events() {
        assert_eq!(
            clip_number("+CLIP: \"+79991234567\",145"),
            Some("+79991234567".into())
        );
        assert!(call_active("+CLCC: 1,0,0,0,0,\"+79991234567\",145"));
        assert!(!call_active("+CLCC: 1,0,2,0,0,\"+79991234567\",145"));
    }
    #[test]
    fn wav_format() {
        let path =
            std::env::temp_dir().join(format!("codex-phone-test-{}.wav", std::process::id()));
        write_input_wav(&path, &[0, 128, 255]).unwrap();
        let wav = hound::WavReader::open(&path).unwrap();
        assert_eq!(wav.spec().sample_rate, 8000);
        assert_eq!(wav.duration(), 3);
        drop(wav);
        let _ = fs::remove_file(path);
    }
}
