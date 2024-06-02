#![forbid(unsafe_code)]
#![allow(unused_imports, dead_code)]
// #![feature(let_chains)]

use async_openai::types::ChatCompletionRequestMessage;
use async_openai::types::Role::{self, *};
use bytes::{BufMut, Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use crossbeam::channel::RecvError;
use deepgram::transcription::live::{Alternatives, Channel, Word as LiveWord};
use deepgram::transcription::prerecorded::response::Word as PrerecordedWord;
use deepgram::{
	transcription::prerecorded::{
		audio_source::AudioSource,
		options::{Language, Options},
	},
	Deepgram, DeepgramError,
};
use egui::text::LayoutJob;
use egui::*;
use egui_node_graph2::*;
use freeverb::Freeverb;
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::stream::StreamExt as _;
use futures::SinkExt;
use once_cell::sync::Lazy;
use poll_promise::Promise;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use std::{env, string, thread};
use stream_cancel::{StreamExt as _, Trigger, Tripwire};
use tokio::fs::File;
use turbosql::*;

mod audiofile;
mod self_update;
mod session;

enum Word {
	Live(LiveWord),
	Prerecorded(PrerecordedWord),
}

impl Word {
	fn speaker(&self) -> usize {
		match self {
			Word::Live(word) => word.speaker as usize,
			Word::Prerecorded(word) => word.speaker.unwrap(),
		}
	}
	fn word(&self) -> &str {
		match self {
			Word::Live(word) => &word.word,
			Word::Prerecorded(word) => &word.word,
		}
	}
}

static TRANSCRIPT: Lazy<Mutex<Vec<Option<LiveWord>>>> = Lazy::new(Default::default);
static TRANSCRIPT_FINAL: Lazy<Mutex<Vec<Option<Word>>>> = Lazy::new(Default::default);
static DURATION: Lazy<Mutex<f64>> = Lazy::new(Default::default);
static COMPLETION: Lazy<Mutex<String>> = Lazy::new(Default::default);
static COMPLETION_PROMPT: Lazy<Mutex<String>> = Lazy::new(|| {
	Mutex::new(String::from(
		"For the personal conversational transcript above, here is a coaching prompt:",
	))
});

#[derive(Clone)]
struct ChatMessage {
	role: Role,
	content: String,
}

struct WheelWindow(Vec<ChatMessage>);

impl Default for WheelWindow {
	fn default() -> Self {
		Self(vec![ChatMessage { role: User, content: String::new() }])
	}
}

static WHEEL_WINDOWS: Lazy<Mutex<Vec<WheelWindow>>> = Lazy::new(Default::default);

#[derive(Turbosql, Default)]
struct Setting {
	rowid: Option<i64>,
	key: String,
	value: String,
}

impl Setting {
	fn get(key: &str) -> Self {
		select!(Setting "WHERE key = " key)
			.unwrap_or(Setting { key: key.to_string(), ..Default::default() })
	}
	fn save(&self) {
		if self.rowid.is_some() {
			self.update().unwrap();
		} else {
			self.insert().unwrap();
		}
	}
}

#[derive(Turbosql, Default)]
struct SampleData {
	rowid: Option<i64>,
	record_ms: i64,
	sample_data: Blob,
}

#[derive(Turbosql, Default)]
struct Card {
	rowid: Option<i64>,
	deleted: bool,
	title: String,
	question: String,
	answer: String,
	last_question_viewed_ms: i64,
	last_answer_viewed_ms: i64,
}

#[allow(clippy::enum_variant_names)]
#[derive(Serialize, Deserialize, Default)]
enum Action {
	#[default]
	NoAction,
	ViewedQuestion,
	ViewedAnswer,
	Responded {
		correct: bool,
	},
}

#[derive(Turbosql, Default)]
struct CardLog {
	rowid: Option<i64>,
	card_id: i64,
	time_ms: i64,
	action: Action,
}

#[derive(Turbosql, Default)]
struct Prompt {
	rowid: Option<i64>,
	time_ms: i64,
	prompt: String,
}

struct Resource {
	/// HTTP response
	response: ehttp::Response,

	text: Option<String>,

	/// If set, the response was an image.
	image: Option<Image<'static>>,

	/// If set, the response was text with some supported syntax highlighting (e.g. ".rs" or ".md").
	colored_text: Option<ColoredText>,
}

impl Resource {
	fn from_response(ctx: &Context, response: ehttp::Response) -> Self {
		let content_type = response.content_type().unwrap_or_default();
		if content_type.starts_with("image/") {
			ctx.include_bytes(response.url.clone(), response.bytes.clone());
			let image = Image::from_uri(response.url.clone());

			Self { response, text: None, colored_text: None, image: Some(image) }
		} else {
			let text = response.text();
			let colored_text = text.and_then(|text| syntax_highlighting(ctx, &response, text));
			let text = text.map(|text| text.to_owned());

			Self { response, text, colored_text, image: None }
		}
	}
}

#[derive(Default, Deserialize, Serialize)]
pub struct App {
	// The `GraphEditorState` is the top-level object. You "register" all your
	// custom types by specifying it as its generic parameters.
	#[serde(skip)]
	state: MyEditorState,

	#[serde(skip)]
	user_state: MyGraphState,

	url: String,
	line_selected: i64,
	title_text: String,
	question_text: String,
	answer_text: String,
	speaker_names: Vec<String>,
	system_text: String,
	prompt_text: String,
	completion_prompt: String,

	#[serde(skip)]
	debounce_tx: Option<Sender<String>>,
	#[serde(skip)]
	gpt_3_trigger: Option<Trigger>,
	#[serde(skip)]
	trigger: Option<Trigger>,
	#[serde(skip)]
	sessions: Vec<session::Session>,
	#[serde(skip)]
	is_recording: bool,
	#[serde(skip)]
	promise: Option<Promise<ehttp::Result<Resource>>>,
}

impl App {
	fn get_transcript(&self) -> String {
		let mut transcript: String = String::new();

		let words = TRANSCRIPT_FINAL.lock().unwrap();

		let lines = words.split(|word| word.is_none());

		for line in lines {
			let mut current_speaker = 100;

			for word in line {
				if let Some(word) = word {
					if word.speaker() != current_speaker {
						current_speaker = word.speaker();
						transcript.push_str(&format!(
							"\n[{}]: ",
							self.speaker_names.get(current_speaker).unwrap_or(&String::new())
						));
					}
					transcript.push_str(&format!("{} ", word.word()));
				}
			}
		}
		transcript.push('\n');
		transcript
	}

	pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
		cc.egui_ctx.set_visuals(egui::style::Visuals::dark());
		cc.egui_ctx.style_mut(|s| s.visuals.override_text_color = Some(Color32::WHITE));

		egui_extras::install_image_loaders(&cc.egui_ctx);

		let (debounce_tx, mut debounce_rx) = mpsc::channel(10);

		let s = Self {
			debounce_tx: Some(debounce_tx),
			sessions: session::Session::calculate_sessions(),
			completion_prompt: COMPLETION_PROMPT.lock().unwrap().clone(),
			..Default::default()
		};

		let ctx_cloned = cc.egui_ctx.clone();

		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_millis(100));
			loop {
				interval.tick().await;
				ctx_cloned.request_repaint();
			}
		});

		let ctx = cc.egui_ctx.clone();

		// Listen for events
		tokio::spawn(async move {
			let duration = Duration::from_millis(300);
			let mut keys_pressed = false;
			let mut string = String::new();
			let mut _trigger = None;

			loop {
				match tokio::time::timeout(duration, debounce_rx.next()).await {
					Ok(Some(s)) => {
						// keyboard activity
						_trigger = None;
						COMPLETION.lock().unwrap().clear();
						string = s;
						keys_pressed = true;
					}
					Ok(None) => {
						eprintln!("Debounce finished");
						break;
					}
					Err(_) => {
						if keys_pressed && !string.is_empty() {
							// eprintln!("{:?} since keyboard activity: {}", duration, &string);
							let (t, tripwire) = Tripwire::new();
							_trigger = Some(t);
							eprintln!("{}", string);
							let string = format!("{} {}", COMPLETION_PROMPT.lock().unwrap(), string);
							let ctx = ctx.clone();
							tokio::spawn(async move {
								COMPLETION.lock().unwrap().clear();
								let ctx = ctx.clone();
								run_openai_completion(tripwire, string, move |content| {
									// eprint!("{}", content);
									COMPLETION.lock().unwrap().push_str(&content);
									ctx.request_repaint();
								})
								.await
								.ok();
							});
							keys_pressed = false;
						}
					}
				}
			}
		});

		// dbg!(&sessions);
		// let session = sessions.first().unwrap();
		// dbg!(session.duration_ms());
		// dbg!(session.samples().len());

		// Load previous app state (if any).
		// Note that you must enable the `persistence` feature for this to work.
		// if let Some(storage) = cc.storage {
		//     return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
		// }

		s
	}
}

trait MyThings {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool;
}

impl MyThings for Ui {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool {
		self
			.add(
				// vec2(400.0, 300.0),
				TextEdit::multiline(text)
					.desired_width(f32::INFINITY)
					// .desired_height(f32::INFINITY)
					.font(FontId::new(30.0, FontFamily::Proportional)),
			)
			.changed()
	}
}

impl eframe::App for App {
	fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
		SidePanel::left("left_panel").show(ctx, |ui| {
			ui.label(option_env!("BUILD_ID").unwrap_or("DEV"));
		});

		let graph_response = egui::CentralPanel::default()
			.show(ctx, |ui| {
				self.state.draw_graph_editor(ui, AllMyNodeTemplates, &mut self.user_state, Vec::default())
			})
			.inner;
		for node_response in graph_response.node_responses {
			// Here, we ignore all other graph events. But you may find
			// some use for them. For example, by playing a sound when a new
			// connection is created
			if let NodeResponse::User(user_event) = node_response {
				match user_event {
					MyResponse::SetActiveNode(node) => self.user_state.active_node = Some(node),
					MyResponse::ClearActiveNode => self.user_state.active_node = None,
				}
			}
		}

		if let Some(node) = self.user_state.active_node {
			if self.state.graph.nodes.contains_key(node) {
				let text = match evaluate_node(&self.state.graph, node, &mut HashMap::new()) {
					Ok(value) => format!("The result is: {:?}", value),
					Err(err) => format!("Execution error: {}", err),
				};
				ctx.debug_painter().text(
					egui::pos2(10.0, 35.0),
					egui::Align2::LEFT_TOP,
					text,
					TextStyle::Button.resolve(&ctx.style()),
					egui::Color32::WHITE,
				);
			} else {
				self.user_state.active_node = None;
			}
		}
	}
}

fn ui_url(ui: &mut Ui, _frame: &mut eframe::Frame, url: &mut String) -> bool {
	let mut trigger_fetch = false;

	ui.horizontal(|ui| {
		ui.label("URL:");
		trigger_fetch |= ui.add(TextEdit::singleline(url).desired_width(f32::INFINITY)).lost_focus();
	});

	ui.horizontal(|ui| {
		if ui.button("Random image").clicked() {
			let seed = ui.input(|i| i.time);
			let side = 640;
			*url = format!("https://picsum.photos/seed/{seed}/{side}");
			trigger_fetch = true;
		}
	});

	trigger_fetch
}

fn ui_resource(ui: &mut Ui, resource: &Resource) {
	let Resource { response, text, image, colored_text } = resource;

	ui.monospace(format!("url:          {}", response.url));
	ui.monospace(format!("status:       {} ({})", response.status, response.status_text));
	ui.monospace(format!("content-type: {}", response.content_type().unwrap_or_default()));
	ui.monospace(format!("size:         {:.1} kB", response.bytes.len() as f32 / 1000.0));

	ui.separator();

	ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
		CollapsingHeader::new("Response headers").default_open(false).show(ui, |ui| {
			Grid::new("response_headers").spacing(vec2(ui.spacing().item_spacing.x * 2.0, 0.0)).show(
				ui,
				|ui| {
					for header in &response.headers {
						ui.label(&header.0);
						ui.label(&header.1);
						ui.end_row();
					}
				},
			)
		});

		ui.separator();

		if let Some(text) = &text {
			let tooltip = "Click to copy the response body";
			if ui.button("📋").on_hover_text(tooltip).clicked() {
				ui.ctx().copy_text(text.clone());
			}
			ui.separator();
		}

		if let Some(image) = image {
			ui.add(image.clone());
		} else if let Some(colored_text) = colored_text {
			colored_text.ui(ui);
		} else if let Some(text) = &text {
			selectable_text(ui, text);
		} else {
			ui.monospace("[binary]");
		}
	});
}

fn selectable_text(ui: &mut Ui, mut text: &str) {
	ui.add(TextEdit::multiline(&mut text).desired_width(f32::INFINITY).font(TextStyle::Monospace));
}

// ----------------------------------------------------------------------------
// Syntax highlighting:

fn syntax_highlighting(
	ctx: &Context,
	response: &ehttp::Response,
	text: &str,
) -> Option<ColoredText> {
	let extension_and_rest: Vec<&str> = response.url.rsplitn(2, '.').collect();
	let extension = extension_and_rest.first()?;
	let theme = egui_extras::syntax_highlighting::CodeTheme::from_style(&ctx.style());
	Some(ColoredText(egui_extras::syntax_highlighting::highlight(ctx, &theme, text, extension)))
}

struct ColoredText(text::LayoutJob);

impl ColoredText {
	pub fn ui(&self, ui: &mut Ui) {
		if true {
			// Selectable text:
			let mut layouter = |ui: &Ui, _string: &str, wrap_width: f32| {
				let mut layout_job = self.0.clone();
				layout_job.wrap.max_width = wrap_width;
				ui.fonts(|f| f.layout_job(layout_job))
			};

			let mut text = self.0.text.as_str();
			ui.add(
				TextEdit::multiline(&mut text)
					.font(TextStyle::Monospace)
					.desired_width(f32::INFINITY)
					.layouter(&mut layouter),
			);
		} else {
			let mut job = self.0.clone();
			job.wrap.max_width = ui.available_width();
			let galley = ui.fonts(|f| f.layout_job(job));
			let (response, painter) = ui.allocate_painter(galley.size(), Sense::hover());
			painter.add(Shape::galley(response.rect.min, galley, ui.visuals().text_color()));
		}
	}
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

	eprintln!("database at {:?}", turbosql::db_path());

	// self_update::self_update().await.ok();

	let stream = stream_setup_for()?;
	stream.play()?;

	eframe::run_native(
		"ace",
		eframe::NativeOptions {
			viewport: egui::ViewportBuilder::default()
				.with_inner_size([400.0, 300.0])
				.with_min_inner_size([300.0, 220.0]),
			..Default::default()
		},
		Box::new(|cc| Box::new(App::new(cc))),
	)?;

	Ok(())
}

fn microphone_as_stream() -> Receiver<Result<Bytes, RecvError>> {
	let (sync_tx, sync_rx) = crossbeam::channel::unbounded();
	let (mut async_tx, async_rx) = mpsc::channel(1);

	thread::spawn(move || {
		let host = cpal::default_host();
		let device = host.default_input_device().unwrap();

		let config = device.supported_input_configs().unwrap();
		for config in config {
			dbg!(&config);
		}

		let config = device.default_input_config().unwrap();

		dbg!(&config);

		let num_channels = config.channels() as usize;

		let stream = match config.sample_format() {
			cpal::SampleFormat::F32 => device
				.build_input_stream(
					&config.into(),
					move |data: &[f32], _: &_| {
						let mut bytes =
							BytesMut::with_capacity((data.len() * std::mem::size_of::<i16>()) / num_channels);
						for sample in data.iter().step_by(num_channels) {
							// if *sample > 0.5 {
							// 	dbg!(sample);
							// }
							bytes.put_i16_le(sample.to_sample::<i16>());
						}
						sync_tx.send(bytes.freeze()).ok();
					},
					|_| panic!(),
					None,
				)
				.unwrap(),
			// cpal::SampleFormat::I16 => device
			// 	.build_input_stream(
			// 		&config.into(),
			// 		move |data: &[i16], _: &_| {
			// 			let mut bytes = BytesMut::with_capacity(data.len() * 2);
			// 			for sample in data {
			// 				bytes.put_i16_le(*sample);
			// 			}
			// 			sync_tx.send(bytes.freeze()).unwrap();
			// 		},
			// 		|_| panic!(),
			// 		None,
			// 	)
			// 	.unwrap(),
			// cpal::SampleFormat::U16 => device
			// 	.build_input_stream(
			// 		&config.into(),
			// 		move |data: &[u16], _: &_| {
			// 			let mut bytes = BytesMut::with_capacity(data.len() * 2);
			// 			for sample in data {
			// 				bytes.put_i16_le(sample.to_sample::<i16>());
			// 			}
			// 			sync_tx.send(bytes.freeze()).unwrap();
			// 		},
			// 		|_| panic!(),
			// 		None,
			// 	)
			// 	.unwrap(),
			_ => panic!("unsupported sample format"),
		};

		stream.play().unwrap();

		loop {
			thread::park();
		}
	});

	tokio::spawn(async move {
		let mut buffer = Vec::with_capacity(150000);
		loop {
			let data = sync_rx.recv();
			if let Ok(data) = &data {
				buffer.extend(data.clone().to_vec());
				if buffer.len() > 100000 {
					let moved_buffer = buffer;
					buffer = Vec::with_capacity(150000);
					tokio::task::spawn_blocking(|| {
						SampleData { rowid: None, record_ms: now_ms(), sample_data: moved_buffer }.insert().unwrap();
					});
				}
			}

			async_tx.send(data).await.unwrap();
		}
	});

	async_rx
}

const GPT_3_5: &str = "gpt-3.5-turbo-0125";
const GPT_4: &str = "gpt-4-0125-preview";

pub(crate) async fn run_openai(
	model: &str,
	tripwire: Tripwire,
	messages: Vec<ChatMessage>,
	transcript: String,
	callback: impl Fn(&String) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
	use async_openai::{types::CreateChatCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::with_config(
		async_openai::config::OpenAIConfig::new().with_api_key(Setting::get("openai_api_key").value),
	);

	let mut messages = messages
		.into_iter()
		.map(|m| match m.role {
			System => async_openai::types::ChatCompletionRequestSystemMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			User => async_openai::types::ChatCompletionRequestUserMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			Assistant => async_openai::types::ChatCompletionRequestAssistantMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			_ => panic!("invalid role"),
		})
		.collect::<Vec<ChatCompletionRequestMessage>>();

	if !transcript.is_empty() {
		messages.insert(
			0,
			async_openai::types::ChatCompletionRequestUserMessageArgs::default()
				.content(transcript)
				.build()
				.unwrap()
				.into(),
		);
	}

	// dbg!(&messages);

	let request = CreateChatCompletionRequestArgs::default()
		.model(model)
		.max_tokens(4096u16)
		.messages(messages)
		.build()?;

	let mut stream = client.chat().create_stream(request).await?.take_until_if(tripwire);

	while let Some(result) = stream.next().await {
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|chat_choice| {
					if let Some(ref content) = chat_choice.delta.content {
						callback(content);
					}
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
	}

	Ok(())
}

pub(crate) async fn run_openai_completion(
	tripwire: Tripwire,
	prompt: String,
	callback: impl Fn(&String) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
	use async_openai::{types::CreateCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::with_config(
		async_openai::config::OpenAIConfig::new().with_api_key(Setting::get("openai_api_key").value),
	);

	let mut logit_bias: HashMap<String, serde_json::Value> = HashMap::new();

	["198", "271", "1432", "4815", "1980", "382", "720", "627"].iter().for_each(|s| {
		logit_bias
			.insert(s.to_string(), serde_json::Value::Number(serde_json::Number::from_f64(-100.).unwrap()));
	});

	let request = CreateCompletionRequestArgs::default()
		.model("gpt-3.5-turbo-instruct")
		.max_tokens(100u16)
		.logit_bias(logit_bias)
		.prompt(prompt)
		.n(1)
		.stream(true)
		.build()?;

	let mut stream = client.completions().create_stream(request).await?.take_until_if(tripwire);

	while let Some(result) = stream.next().await {
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|c| {
					callback(&c.text);
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
	}

	Ok(())
}

// ========= First, define your user data types =============

/// The NodeData holds a custom data struct inside each node. It's useful to
/// store additional information that doesn't live in parameters. For this
/// example, the node data stores the template (i.e. the "type") of the node.
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub struct MyNodeData {
	template: MyNodeTemplate,
}

/// `DataType`s are what defines the possible range of connections when
/// attaching two ports together. The graph UI will make sure to not allow
/// attaching incompatible datatypes.
#[derive(PartialEq, Eq)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub enum MyDataType {
	Scalar,
	Vec2,
	Text,
}

/// In the graph, input parameters can optionally have a constant value. This
/// value can be directly edited in a widget inside the node itself.
///
/// There will usually be a correspondence between DataTypes and ValueTypes. But
/// this library makes no attempt to check this consistency. For instance, it is
/// up to the user code in this example to make sure no parameter is created
/// with a DataType of Scalar and a ValueType of Vec2.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub enum MyValueType {
	Vec2 { value: egui::Vec2 },
	Scalar { value: f32 },
	Text { value: String },
}

impl Default for MyValueType {
	fn default() -> Self {
		// NOTE: This is just a dummy `Default` implementation. The library
		// requires it to circumvent some internal borrow checker issues.
		Self::Scalar { value: 0.0 }
	}
}

impl MyValueType {
	/// Tries to downcast this value type to a vector
	pub fn try_to_vec2(self) -> anyhow::Result<egui::Vec2> {
		if let MyValueType::Vec2 { value } = self {
			Ok(value)
		} else {
			anyhow::bail!("Invalid cast from {:?} to vec2", self)
		}
	}

	/// Tries to downcast this value type to a scalar
	pub fn try_to_scalar(self) -> anyhow::Result<f32> {
		if let MyValueType::Scalar { value } = self {
			Ok(value)
		} else {
			anyhow::bail!("Invalid cast from {:?} to scalar", self)
		}
	}

	pub fn try_to_text(self) -> anyhow::Result<String> {
		if let MyValueType::Text { value } = self {
			Ok(value)
		} else {
			anyhow::bail!("Invalid cast from {:?} to text", self)
		}
	}
}

/// NodeTemplate is a mechanism to define node templates. It's what the graph
/// will display in the "new node" popup. The user code needs to tell the
/// library how to convert a NodeTemplate into a Node.
#[derive(Clone, Copy)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub enum MyNodeTemplate {
	MakeText,
	MakeScalar,
	AddScalar,
	SubtractScalar,
	MakeVector,
	AddVector,
	SubtractVector,
	VectorTimesScalar,
}

/// The response type is used to encode side-effects produced when drawing a
/// node in the graph. Most side-effects (creating new nodes, deleting existing
/// nodes, handling connections...) are already handled by the library, but this
/// mechanism allows creating additional side effects from user code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MyResponse {
	SetActiveNode(NodeId),
	ClearActiveNode,
}

/// The graph 'global' state. This state struct is passed around to the node and
/// parameter drawing callbacks. The contents of this struct are entirely up to
/// the user. For this example, we use it to keep track of the 'active' node.
#[derive(Default)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub struct MyGraphState {
	pub active_node: Option<NodeId>,
}

// =========== Then, you need to implement some traits ============

// A trait for the data types, to tell the library how to display them
impl DataTypeTrait<MyGraphState> for MyDataType {
	fn data_type_color(&self, _user_state: &mut MyGraphState) -> egui::Color32 {
		match self {
			MyDataType::Scalar => egui::Color32::from_rgb(38, 109, 211),
			MyDataType::Vec2 => egui::Color32::from_rgb(238, 207, 109),
			MyDataType::Text => egui::Color32::from_rgb(238, 238, 238),
		}
	}

	fn name(&self) -> Cow<'_, str> {
		match self {
			MyDataType::Scalar => Cow::Borrowed("scalar"),
			MyDataType::Vec2 => Cow::Borrowed("2d vector"),
			MyDataType::Text => Cow::Borrowed("text"),
		}
	}
}

// A trait for the node kinds, which tells the library how to build new nodes
// from the templates in the node finder
impl NodeTemplateTrait for MyNodeTemplate {
	type NodeData = MyNodeData;
	type DataType = MyDataType;
	type ValueType = MyValueType;
	type UserState = MyGraphState;
	type CategoryType = &'static str;

	fn node_finder_label(&self, _user_state: &mut Self::UserState) -> Cow<'_, str> {
		Cow::Borrowed(match self {
			MyNodeTemplate::MakeText => "New text",
			MyNodeTemplate::MakeScalar => "New scalar",
			MyNodeTemplate::AddScalar => "Scalar add",
			MyNodeTemplate::SubtractScalar => "Scalar subtract",
			MyNodeTemplate::MakeVector => "New vector",
			MyNodeTemplate::AddVector => "Vector add",
			MyNodeTemplate::SubtractVector => "Vector subtract",
			MyNodeTemplate::VectorTimesScalar => "Vector times scalar",
		})
	}

	// this is what allows the library to show collapsible lists in the node finder.
	fn node_finder_categories(&self, _user_state: &mut Self::UserState) -> Vec<&'static str> {
		match self {
			MyNodeTemplate::MakeText => vec!["Text"],
			MyNodeTemplate::MakeScalar | MyNodeTemplate::AddScalar | MyNodeTemplate::SubtractScalar => {
				vec!["Scalar"]
			}
			MyNodeTemplate::MakeVector | MyNodeTemplate::AddVector | MyNodeTemplate::SubtractVector => {
				vec!["Vector"]
			}
			MyNodeTemplate::VectorTimesScalar => vec!["Vector", "Scalar"],
		}
	}

	fn node_graph_label(&self, user_state: &mut Self::UserState) -> String {
		// It's okay to delegate this to node_finder_label if you don't want to
		// show different names in the node finder and the node itself.
		self.node_finder_label(user_state).into()
	}

	fn user_data(&self, _user_state: &mut Self::UserState) -> Self::NodeData {
		MyNodeData { template: *self }
	}

	fn build_node(
		&self,
		graph: &mut Graph<Self::NodeData, Self::DataType, Self::ValueType>,
		_user_state: &mut Self::UserState,
		node_id: NodeId,
	) {
		// The nodes are created empty by default. This function needs to take
		// care of creating the desired inputs and outputs based on the template

		// We define some closures here to avoid boilerplate. Note that this is
		// entirely optional.
		let input_scalar = |graph: &mut MyGraph, name: &str| {
			graph.add_input_param(
				node_id,
				name.to_string(),
				MyDataType::Scalar,
				MyValueType::Scalar { value: 0.0 },
				InputParamKind::ConnectionOrConstant,
				true,
			);
		};
		let input_text = |graph: &mut MyGraph, name: &str| {
			graph.add_input_param(
				node_id,
				name.to_string(),
				MyDataType::Text,
				MyValueType::Text { value: "text goes here".to_string() },
				InputParamKind::ConnectionOrConstant,
				true,
			);
		};
		let input_vector = |graph: &mut MyGraph, name: &str| {
			graph.add_input_param(
				node_id,
				name.to_string(),
				MyDataType::Vec2,
				MyValueType::Vec2 { value: egui::vec2(0.0, 0.0) },
				InputParamKind::ConnectionOrConstant,
				true,
			);
		};

		let output_scalar = |graph: &mut MyGraph, name: &str| {
			graph.add_output_param(node_id, name.to_string(), MyDataType::Scalar);
		};
		let output_text = |graph: &mut MyGraph, name: &str| {
			graph.add_output_param(node_id, name.to_string(), MyDataType::Text);
		};
		let output_vector = |graph: &mut MyGraph, name: &str| {
			graph.add_output_param(node_id, name.to_string(), MyDataType::Vec2);
		};

		match self {
			MyNodeTemplate::AddScalar => {
				// The first input param doesn't use the closure so we can comment
				// it in more detail.
				graph.add_input_param(
					node_id,
					// This is the name of the parameter. Can be later used to
					// retrieve the value. Parameter names should be unique.
					"A".into(),
					// The data type for this input. In this case, a scalar
					MyDataType::Scalar,
					// The value type for this input. We store zero as default
					MyValueType::Scalar { value: 0.0 },
					// The input parameter kind. This allows defining whether a
					// parameter accepts input connections and/or an inline
					// widget to set its value.
					InputParamKind::ConnectionOrConstant,
					true,
				);
				input_scalar(graph, "B");
				output_scalar(graph, "out");
			}
			MyNodeTemplate::SubtractScalar => {
				input_scalar(graph, "A");
				input_scalar(graph, "B");
				output_scalar(graph, "out");
			}
			MyNodeTemplate::VectorTimesScalar => {
				input_scalar(graph, "scalar");
				input_vector(graph, "vector");
				output_vector(graph, "out");
			}
			MyNodeTemplate::AddVector => {
				input_vector(graph, "v1");
				input_vector(graph, "v2");
				output_vector(graph, "out");
			}
			MyNodeTemplate::SubtractVector => {
				input_vector(graph, "v1");
				input_vector(graph, "v2");
				output_vector(graph, "out");
			}
			MyNodeTemplate::MakeVector => {
				input_scalar(graph, "x");
				input_scalar(graph, "y");
				output_vector(graph, "out");
			}
			MyNodeTemplate::MakeScalar => {
				input_scalar(graph, "value");
				output_scalar(graph, "out");
			}
			MyNodeTemplate::MakeText => {
				input_text(graph, "value");
				output_text(graph, "out");
			}
		}
	}
}

pub struct AllMyNodeTemplates;
impl NodeTemplateIter for AllMyNodeTemplates {
	type Item = MyNodeTemplate;

	fn all_kinds(&self) -> Vec<Self::Item> {
		// This function must return a list of node kinds, which the node finder
		// will use to display it to the user. Crates like strum can reduce the
		// boilerplate in enumerating all variants of an enum.
		vec![
			MyNodeTemplate::MakeText,
			MyNodeTemplate::MakeScalar,
			MyNodeTemplate::MakeVector,
			MyNodeTemplate::AddScalar,
			MyNodeTemplate::SubtractScalar,
			MyNodeTemplate::AddVector,
			MyNodeTemplate::SubtractVector,
			MyNodeTemplate::VectorTimesScalar,
		]
	}
}

impl WidgetValueTrait for MyValueType {
	type Response = MyResponse;
	type UserState = MyGraphState;
	type NodeData = MyNodeData;
	fn value_widget(
		&mut self,
		param_name: &str,
		_node_id: NodeId,
		ui: &mut egui::Ui,
		_user_state: &mut MyGraphState,
		_node_data: &MyNodeData,
	) -> Vec<MyResponse> {
		// This trait is used to tell the library which UI to display for the
		// inline parameter widgets.
		match self {
			MyValueType::Vec2 { value } => {
				ui.label(param_name);
				ui.horizontal(|ui| {
					ui.label("x");
					ui.add(DragValue::new(&mut value.x));
					ui.label("y");
					ui.add(DragValue::new(&mut value.y));
				});
			}
			MyValueType::Text { value } => {
				ui.add(
					egui::TextEdit::multiline(value)
						// .id(id)
						.lock_focus(true)
						// .font(FontId::new(20.0, FontFamily::Monospace))
						.desired_width(f32::INFINITY), // .layouter(&mut layouter),
				);
			}
			MyValueType::Scalar { value } => {
				ui.horizontal(|ui| {
					ui.label(param_name);
					ui.add(DragValue::new(value));
				});
			}
		}
		// This allows you to return your responses from the inline widgets.
		Vec::new()
	}
}

impl UserResponseTrait for MyResponse {}
impl NodeDataTrait for MyNodeData {
	type Response = MyResponse;
	type UserState = MyGraphState;
	type DataType = MyDataType;
	type ValueType = MyValueType;

	// This method will be called when drawing each node. This allows adding
	// extra ui elements inside the nodes. In this case, we create an "active"
	// button which introduces the concept of having an active node in the
	// graph. This is done entirely from user code with no modifications to the
	// node graph library.
	fn bottom_ui(
		&self,
		ui: &mut egui::Ui,
		node_id: NodeId,
		_graph: &Graph<MyNodeData, MyDataType, MyValueType>,
		user_state: &mut Self::UserState,
	) -> Vec<NodeResponse<MyResponse, MyNodeData>>
	where
		MyResponse: UserResponseTrait,
	{
		// This logic is entirely up to the user. In this case, we check if the
		// current node we're drawing is the active one, by comparing against
		// the value stored in the global user state, and draw different button
		// UIs based on that.

		let mut responses = vec![];
		let is_active = user_state.active_node.map(|id| id == node_id).unwrap_or(false);

		// Pressing the button will emit a custom user response to either set,
		// or clear the active node. These responses do nothing by themselves,
		// the library only makes the responses available to you after the graph
		// has been drawn. See below at the update method for an example.
		if !is_active {
			if ui.button("👁 Set active").clicked() {
				responses.push(NodeResponse::User(MyResponse::SetActiveNode(node_id)));
			}
		} else {
			let button = egui::Button::new(egui::RichText::new("👁 Active").color(egui::Color32::BLACK))
				.fill(egui::Color32::GOLD);
			if ui.add(button).clicked() {
				responses.push(NodeResponse::User(MyResponse::ClearActiveNode));
			}
		}

		responses
	}
}

type MyGraph = Graph<MyNodeData, MyDataType, MyValueType>;
type MyEditorState =
	GraphEditorState<MyNodeData, MyDataType, MyValueType, MyNodeTemplate, MyGraphState>;

#[derive(Default)]
pub struct NodeGraphExample {
	// The `GraphEditorState` is the top-level object. You "register" all your
	// custom types by specifying it as its generic parameters.
	state: MyEditorState,

	user_state: MyGraphState,
}

#[cfg(feature = "persistence")]
const PERSISTENCE_KEY: &str = "egui_node_graph";

#[cfg(feature = "persistence")]
impl NodeGraphExample {
	/// If the persistence feature is enabled, Called once before the first frame.
	/// Load previous app state (if any).
	pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
		let state =
			cc.storage.and_then(|storage| eframe::get_value(storage, PERSISTENCE_KEY)).unwrap_or_default();
		Self { state, user_state: MyGraphState::default() }
	}
}

impl eframe::App for NodeGraphExample {
	#[cfg(feature = "persistence")]
	/// If the persistence function is enabled,
	/// Called by the frame work to save state before shutdown.
	fn save(&mut self, storage: &mut dyn eframe::Storage) {
		eframe::set_value(storage, PERSISTENCE_KEY, &self.state);
	}
	/// Called each time the UI needs repainting, which may be many times per second.
	/// Put your widgets into a `SidePanel`, `TopPanel`, `CentralPanel`, `Window` or `Area`.
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		egui::TopBottomPanel::top("top").show(ctx, |ui| {
			egui::menu::bar(ui, |ui| {
				egui::widgets::global_dark_light_mode_switch(ui);
			});
		});
		let graph_response = egui::CentralPanel::default()
			.show(ctx, |ui| {
				self.state.draw_graph_editor(ui, AllMyNodeTemplates, &mut self.user_state, Vec::default())
			})
			.inner;
		for node_response in graph_response.node_responses {
			// Here, we ignore all other graph events. But you may find
			// some use for them. For example, by playing a sound when a new
			// connection is created
			if let NodeResponse::User(user_event) = node_response {
				match user_event {
					MyResponse::SetActiveNode(node) => self.user_state.active_node = Some(node),
					MyResponse::ClearActiveNode => self.user_state.active_node = None,
				}
			}
		}

		if let Some(node) = self.user_state.active_node {
			if self.state.graph.nodes.contains_key(node) {
				let text = match evaluate_node(&self.state.graph, node, &mut HashMap::new()) {
					Ok(value) => format!("The result is: {:?}", value),
					Err(err) => format!("Execution error: {}", err),
				};
				ctx.debug_painter().text(
					egui::pos2(10.0, 35.0),
					egui::Align2::LEFT_TOP,
					text,
					TextStyle::Button.resolve(&ctx.style()),
					egui::Color32::WHITE,
				);
			} else {
				self.user_state.active_node = None;
			}
		}
	}
}

type OutputsCache = HashMap<OutputId, MyValueType>;

/// Recursively evaluates all dependencies of this node, then evaluates the node itself.
pub fn evaluate_node(
	graph: &MyGraph,
	node_id: NodeId,
	outputs_cache: &mut OutputsCache,
) -> anyhow::Result<MyValueType> {
	// To solve a similar problem as creating node types above, we define an
	// Evaluator as a convenience. It may be overkill for this small example,
	// but something like this makes the code much more readable when the
	// number of nodes starts growing.

	struct Evaluator<'a> {
		graph: &'a MyGraph,
		outputs_cache: &'a mut OutputsCache,
		node_id: NodeId,
	}
	impl<'a> Evaluator<'a> {
		fn new(graph: &'a MyGraph, outputs_cache: &'a mut OutputsCache, node_id: NodeId) -> Self {
			Self { graph, outputs_cache, node_id }
		}
		fn evaluate_input(&mut self, name: &str) -> anyhow::Result<MyValueType> {
			// Calling `evaluate_input` recursively evaluates other nodes in the
			// graph until the input value for a paramater has been computed.
			evaluate_input(self.graph, self.node_id, name, self.outputs_cache)
		}
		fn populate_output(&mut self, name: &str, value: MyValueType) -> anyhow::Result<MyValueType> {
			// After computing an output, we don't just return it, but we also
			// populate the outputs cache with it. This ensures the evaluation
			// only ever computes an output once.
			//
			// The return value of the function is the "final" output of the
			// node, the thing we want to get from the evaluation. The example
			// would be slightly more contrived when we had multiple output
			// values, as we would need to choose which of the outputs is the
			// one we want to return. Other outputs could be used as
			// intermediate values.
			//
			// Note that this is just one possible semantic interpretation of
			// the graphs, you can come up with your own evaluation semantics!
			populate_output(self.graph, self.outputs_cache, self.node_id, name, value)
		}
		fn input_vector(&mut self, name: &str) -> anyhow::Result<egui::Vec2> {
			self.evaluate_input(name)?.try_to_vec2()
		}
		fn input_scalar(&mut self, name: &str) -> anyhow::Result<f32> {
			self.evaluate_input(name)?.try_to_scalar()
		}
		fn input_text(&mut self, name: &str) -> anyhow::Result<String> {
			self.evaluate_input(name)?.try_to_text()
		}
		fn output_vector(&mut self, name: &str, value: egui::Vec2) -> anyhow::Result<MyValueType> {
			self.populate_output(name, MyValueType::Vec2 { value })
		}
		fn output_scalar(&mut self, name: &str, value: f32) -> anyhow::Result<MyValueType> {
			self.populate_output(name, MyValueType::Scalar { value })
		}
		fn output_text(&mut self, name: &str, value: String) -> anyhow::Result<MyValueType> {
			self.populate_output(name, MyValueType::Text { value })
		}
	}

	let node = &graph[node_id];
	let mut evaluator = Evaluator::new(graph, outputs_cache, node_id);
	match node.user_data.template {
		MyNodeTemplate::AddScalar => {
			let a = evaluator.input_scalar("A")?;
			let b = evaluator.input_scalar("B")?;
			evaluator.output_scalar("out", a + b)
		}
		MyNodeTemplate::SubtractScalar => {
			let a = evaluator.input_scalar("A")?;
			let b = evaluator.input_scalar("B")?;
			evaluator.output_scalar("out", a - b)
		}
		MyNodeTemplate::VectorTimesScalar => {
			let scalar = evaluator.input_scalar("scalar")?;
			let vector = evaluator.input_vector("vector")?;
			evaluator.output_vector("out", vector * scalar)
		}
		MyNodeTemplate::AddVector => {
			let v1 = evaluator.input_vector("v1")?;
			let v2 = evaluator.input_vector("v2")?;
			evaluator.output_vector("out", v1 + v2)
		}
		MyNodeTemplate::SubtractVector => {
			let v1 = evaluator.input_vector("v1")?;
			let v2 = evaluator.input_vector("v2")?;
			evaluator.output_vector("out", v1 - v2)
		}
		MyNodeTemplate::MakeVector => {
			let x = evaluator.input_scalar("x")?;
			let y = evaluator.input_scalar("y")?;
			evaluator.output_vector("out", egui::vec2(x, y))
		}
		MyNodeTemplate::MakeScalar => {
			let value = evaluator.input_scalar("value")?;
			evaluator.output_scalar("out", value)
		}
		MyNodeTemplate::MakeText => {
			let value = evaluator.input_text("value")?;
			evaluator.output_text("out", value)
		}
	}
}

fn populate_output(
	graph: &MyGraph,
	outputs_cache: &mut OutputsCache,
	node_id: NodeId,
	param_name: &str,
	value: MyValueType,
) -> anyhow::Result<MyValueType> {
	let output_id = graph[node_id].get_output(param_name)?;
	outputs_cache.insert(output_id, value.clone());
	Ok(value)
}

// Evaluates the input value of
fn evaluate_input(
	graph: &MyGraph,
	node_id: NodeId,
	param_name: &str,
	outputs_cache: &mut OutputsCache,
) -> anyhow::Result<MyValueType> {
	let input_id = graph[node_id].get_input(param_name)?;

	// The output of another node is connected.
	if let Some(other_output_id) = graph.connection(input_id) {
		// The value was already computed due to the evaluation of some other
		// node. We simply return value from the cache.
		if let Some(other_value) = outputs_cache.get(&other_output_id) {
			Ok((*other_value).clone())
		}
		// This is the first time encountering this node, so we need to
		// recursively evaluate it.
		else {
			// Calling this will populate the cache
			evaluate_node(graph, graph[other_output_id].node, outputs_cache)?;

			// Now that we know the value is cached, return it
			Ok(outputs_cache.get(&other_output_id).cloned().expect("Cache should be populated"))
		}
	}
	// No existing connection, take the inline value instead.
	else {
		Ok(graph[input_id].value.clone())
	}
}

pub enum Waveform {
	Sine,
	Square,
	Saw,
	Triangle,
}

pub struct Oscillator {
	pub sample_rate: f32,
	pub waveform: Waveform,
	pub current_sample_index: f32,
	pub frequency_hz: f32,
	pub freeverb: Freeverb,
}

impl Oscillator {
	fn advance_sample(&mut self) {
		self.current_sample_index = (self.current_sample_index + 1.0) % self.sample_rate;
	}

	fn set_waveform(&mut self, waveform: Waveform) {
		self.waveform = waveform;
	}

	fn calculate_sine_output_from_freq(&self, freq: f32) -> f32 {
		let two_pi = 2.0 * std::f32::consts::PI;
		(self.current_sample_index * freq * two_pi / self.sample_rate).sin()
	}

	fn is_multiple_of_freq_above_nyquist(&self, multiple: f32) -> bool {
		self.frequency_hz * multiple > self.sample_rate / 2.0
	}

	fn sine_wave(&mut self) -> f32 {
		self.advance_sample();
		self.calculate_sine_output_from_freq(self.frequency_hz)
	}

	fn generative_waveform(&mut self, harmonic_index_increment: i32, gain_exponent: f32) -> f32 {
		self.advance_sample();
		let mut output = 0.0;
		let mut i = 1;
		while !self.is_multiple_of_freq_above_nyquist(i as f32) {
			let gain = 1.0 / (i as f32).powf(gain_exponent);
			output += gain * self.calculate_sine_output_from_freq(self.frequency_hz * i as f32);
			i += harmonic_index_increment;
		}
		output
	}

	fn square_wave(&mut self) -> f32 {
		self.generative_waveform(2, 1.0)
	}

	fn saw_wave(&mut self) -> f32 {
		self.generative_waveform(1, 1.0)
	}

	fn triangle_wave(&mut self) -> f32 {
		self.generative_waveform(2, 2.0)
	}

	fn tick(&mut self) -> f32 {
		match self.waveform {
			Waveform::Sine => self.sine_wave(),
			Waveform::Square => self.square_wave(),
			Waveform::Saw => self.saw_wave(),
			Waveform::Triangle => self.triangle_wave(),
		}
	}
}

pub fn stream_setup_for() -> Result<cpal::Stream, anyhow::Error>
where
{
	let (_host, device, config) = host_device_setup()?;

	match config.sample_format() {
		cpal::SampleFormat::I8 => make_stream::<i8>(&device, &config.into()),
		cpal::SampleFormat::I16 => make_stream::<i16>(&device, &config.into()),
		cpal::SampleFormat::I32 => make_stream::<i32>(&device, &config.into()),
		cpal::SampleFormat::I64 => make_stream::<i64>(&device, &config.into()),
		cpal::SampleFormat::U8 => make_stream::<u8>(&device, &config.into()),
		cpal::SampleFormat::U16 => make_stream::<u16>(&device, &config.into()),
		cpal::SampleFormat::U32 => make_stream::<u32>(&device, &config.into()),
		cpal::SampleFormat::U64 => make_stream::<u64>(&device, &config.into()),
		cpal::SampleFormat::F32 => make_stream::<f32>(&device, &config.into()),
		cpal::SampleFormat::F64 => make_stream::<f64>(&device, &config.into()),
		sample_format => Err(anyhow::Error::msg(format!("Unsupported sample format '{sample_format}'"))),
	}
}

pub fn host_device_setup(
) -> Result<(cpal::Host, cpal::Device, cpal::SupportedStreamConfig), anyhow::Error> {
	let host = cpal::default_host();

	let device = host
		.default_output_device()
		.ok_or_else(|| anyhow::Error::msg("Default output device is not available"))?;
	println!("Output device : {}", device.name()?);

	let config = device.default_output_config()?;
	println!("Default output config : {:?}", config);

	Ok((host, device, config))
}

pub fn make_stream<T>(
	device: &cpal::Device,
	config: &cpal::StreamConfig,
) -> Result<cpal::Stream, anyhow::Error>
where
	T: SizedSample + FromSample<f32>,
{
	let num_channels = config.channels as usize;
	let mut oscillator = Oscillator {
		waveform: Waveform::Sine,
		sample_rate: config.sample_rate.0 as f32,
		current_sample_index: 0.0,
		frequency_hz: 440.0,
		freeverb: Freeverb::new(44100),
	};

	let err_fn = |err| eprintln!("Error building output sound stream: {}", err);

	let time_at_start = std::time::Instant::now();
	println!("Time at start: {:?}", time_at_start);

	let stream = device.build_output_stream(
		config,
		move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
			// for 0-1s play sine, 1-2s play square, 2-3s play saw, 3-4s play triangle_wave
			let time_since_start = std::time::Instant::now().duration_since(time_at_start).as_secs_f32();
			if time_since_start < 1.0 {
				oscillator.set_waveform(Waveform::Sine);
			} else if time_since_start < 2.0 {
				oscillator.set_waveform(Waveform::Triangle);
			} else if time_since_start < 3.0 {
				oscillator.set_waveform(Waveform::Square);
			} else if time_since_start < 4.0 {
				oscillator.set_waveform(Waveform::Saw);
			} else {
				oscillator.set_waveform(Waveform::Sine);
			}
			process_frame(output, &mut oscillator, num_channels)
		},
		err_fn,
		None,
	)?;

	Ok(stream)
}

fn process_frame<SampleType>(
	output: &mut [SampleType],
	oscillator: &mut Oscillator,
	num_channels: usize,
) where
	SampleType: Sample + FromSample<f32>,
{
	for frame in output.chunks_mut(num_channels) {
		let sample = oscillator.tick();
		let verbed_sample = oscillator.freeverb.tick((sample.into(), sample.into()));

		// let value: SampleType = SampleType::from_sample(oscillator.tick());

		frame[0] = SampleType::from_sample(verbed_sample.0 as f32);
		frame[1] = SampleType::from_sample(verbed_sample.1 as f32);
		// copy the same value to all channels
		// for sample in frame.iter_mut() {
		// 	*sample = value;
		// }
	}
}
