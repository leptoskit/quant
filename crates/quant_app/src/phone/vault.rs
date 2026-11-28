//! The Vault segment: browse every permitted stream (including custom streams)
//! grouped by family, switch ranges, and inspect per-stream numeric values with
//! exact units, duration breakdowns, or discrete record counts.
//!
//! All data lives strictly in memory and clears whenever companion access or
//! vault identity changes. Stale responses after changing range, stream, or
//! vault are ignored via monotonic request counters.

use leptoskit::native::reactive::For;
use leptoskit::prelude::ActivityIndicator;
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::GetUntracked;
use leptoskit::prelude::Set;
use leptoskit::prelude::Text;
use leptoskit::prelude::View;
use quant_model::phone::CompanionCapability;
use quant_model::phone::StatsAccess;
use quant_model::phone::StatsBucket;
use quant_model::phone::StatsChannel;
use quant_model::phone::StatsDetail;
use quant_model::phone::StatsDuration;
use quant_model::phone::StatsStream;
use quant_model::phone::StatsWindow;

use super::State;
use super::companion;
use crate::theme;

/// In-memory session state for the Vault segment. Never written to disk or the
/// collector spool.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VaultUiState {
	/// Selected time range (`Past 24 hours` or `Past 7 days`).
	pub window: StatsWindow,
	/// Monotonic sequence for catalog page requests; stale replies are ignored.
	pub catalog_seq: u64,
	/// Whether a catalog page request is in flight.
	pub loading_catalog: bool,
	/// Whether a "load more" page request is in flight.
	pub loading_more: bool,
	/// Whether at least one catalog response succeeded for the current range.
	pub loaded: bool,
	/// Effective range start reported by the vault.
	pub from: Option<String>,
	/// Effective range end reported by the vault.
	pub to: Option<String>,
	/// When the vault last answered.
	pub fetched: Option<String>,
	/// Total permitted streams in the vault catalog.
	pub total_streams: usize,
	/// Next catalog offset when more permitted streams remain.
	pub next_offset: Option<usize>,
	/// Loaded streams across pages (including custom and empty streams).
	pub streams: Vec<StatsStream>,
	/// Last catalog error, if any.
	pub error: Option<String>,
	/// Currently opened stream id, when inspecting per-stream detail.
	pub selected_stream: Option<String>,
	/// Monotonic sequence for detail requests; stale replies are ignored.
	pub detail_seq: u64,
	/// Whether an on-demand stream detail request is in flight.
	pub loading_detail: bool,
	/// Loaded detail for `selected_stream`.
	pub detail: Option<StatsStream>,
	/// Effective range start for the opened stream detail.
	pub detail_from: Option<String>,
	/// Effective range end for the opened stream detail.
	pub detail_to: Option<String>,
	/// When the detail was fetched.
	pub detail_fetched: Option<String>,
	/// Total channels on the opened numeric stream.
	pub total_channels: usize,
	/// Next channel offset when more channels remain.
	pub next_channel_offset: Option<usize>,
	/// Total duration groups on the opened event stream.
	pub total_groups: usize,
	/// Next duration-group offset when more groups remain.
	pub next_group_offset: Option<usize>,
	/// Last stream detail error, if any.
	pub detail_error: Option<String>,
}

impl VaultUiState {
	/// Clear all session data and bump request sequences so any in-flight
	/// response for an earlier vault, grant, or range is ignored.
	pub fn clear_session(&mut self) {
		self.catalog_seq = self.catalog_seq.wrapping_add(1);
		self.detail_seq = self.detail_seq.wrapping_add(1);
		self.loading_catalog = false;
		self.loading_more = false;
		self.loaded = false;
		self.from = None;
		self.to = None;
		self.fetched = None;
		self.total_streams = 0;
		self.next_offset = None;
		self.streams.clear();
		self.error = None;
		self.selected_stream = None;
		self.loading_detail = false;
		self.detail = None;
		self.detail_from = None;
		self.detail_to = None;
		self.detail_fetched = None;
		self.total_channels = 0;
		self.next_channel_offset = None;
		self.total_groups = 0;
		self.next_group_offset = None;
		self.detail_error = None;
	}
}

/// The Vault segment root.
pub(crate) fn vault_segment(state: &State) -> Element {
	let snapshot = state.snapshot;
	let vault_ui = state.vault_ui;
	let state_for_access = state.clone();
	let state_for_body = state.clone();
	View()
		.class("gap-5")
		.child(companion::access_card(
			&state_for_access,
			CompanionCapability::StatsSummary,
		))
		.child(For(
			move || {
				let access = snapshot.get().stats.access;
				if access != StatsAccess::Ready {
					return Vec::new();
				}
				let ui = vault_ui.get();
				vec![VaultViewKey {
					window: ui.window,
					loading_catalog: ui.loading_catalog,
					loading_more: ui.loading_more,
					loaded: ui.loaded,
					from: ui.from,
					to: ui.to,
					fetched: ui.fetched,
					total_streams: ui.total_streams,
					next_offset: ui.next_offset,
					streams_len: ui.streams.len(),
					catalog_seq: ui.catalog_seq,
					error: ui.error,
					selected_stream: ui.selected_stream,
					detail_seq: ui.detail_seq,
					loading_detail: ui.loading_detail,
					detail_present: ui.detail.is_some(),
					detail_from: ui.detail_from,
					detail_to: ui.detail_to,
					detail_fetched: ui.detail_fetched,
					total_channels: ui.total_channels,
					next_channel_offset: ui.next_channel_offset,
					total_groups: ui.total_groups,
					next_group_offset: ui.next_group_offset,
					detail_error: ui.detail_error,
				}]
			},
			Clone::clone,
			move |_| {
				let ui = state_for_body.vault_ui.get_untracked();
				if ui.selected_stream.is_some() {
					detail_view(&state_for_body, &ui)
				} else {
					catalog_view(&state_for_body, &ui)
				}
			},
		))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct VaultViewKey {
	window: StatsWindow,
	loading_catalog: bool,
	loading_more: bool,
	loaded: bool,
	from: Option<String>,
	to: Option<String>,
	fetched: Option<String>,
	total_streams: usize,
	next_offset: Option<usize>,
	streams_len: usize,
	catalog_seq: u64,
	error: Option<String>,
	selected_stream: Option<String>,
	detail_seq: u64,
	loading_detail: bool,
	detail_present: bool,
	detail_from: Option<String>,
	detail_to: Option<String>,
	detail_fetched: Option<String>,
	total_channels: usize,
	next_channel_offset: Option<usize>,
	total_groups: usize,
	next_group_offset: Option<usize>,
	detail_error: Option<String>,
}

fn range_toolbar(state: &State, active: StatsWindow, show_manage: bool) -> Element {
	let day_state = state.clone();
	let week_state = state.clone();
	let refresh_state = state.clone();
	let access_state = state.clone();
	View()
		.class("gap-3")
		.child(
			View()
				.class("flex-row items-center gap-2")
				.child(
					Button("24 hours")
						.class(if active == StatsWindow::Day {
							format!("flex-1 {}", theme::COMPACT_ACTIVE)
						} else {
							format!("flex-1 {}", theme::COMPACT)
						})
						.accessibility_label("Show past 24 hours")
						.test_id("range-day")
						.on_press(move || day_state.set_stats_window(StatsWindow::Day)),
				)
				.child(
					Button("7 days")
						.class(if active == StatsWindow::Week {
							format!("flex-1 {}", theme::COMPACT_ACTIVE)
						} else {
							format!("flex-1 {}", theme::COMPACT)
						})
						.accessibility_label("Show past 7 days")
						.test_id("range-week")
						.on_press(move || week_state.set_stats_window(StatsWindow::Week)),
				),
		)
		.child(
			View()
				.class("flex-row items-center justify-between gap-2")
				.child(
					Button("Refresh")
						.class(theme::COMPACT)
						.test_id("vault-refresh")
						.on_press(move || refresh_state.reload_vault()),
				)
				.children(show_manage.then(|| {
					Button("Manage access")
						.class(theme::QUIET)
						.test_id("vault-manage-access")
						.on_press(move || {
							let now = !access_state.manage_access.get_untracked();
							if now {
								access_state
									.sync_capability_toggles(CompanionCapability::StatsSummary);
							}
							access_state.manage_access.set(now);
						})
				})),
		)
}

fn catalog_view(state: &State, ui: &VaultUiState) -> Element {
	let mut root = View().class("gap-3").child(
		View()
			.class("gap-3 p-4 rounded-3xl border border-[#2A2560] bg-[#17143A]")
			.child(Text(ui.window.label()).class(theme::HEADING))
			.child(range_toolbar(state, ui.window, true))
			.children(range_meta(
				ui.from.as_deref(),
				ui.to.as_deref(),
				ui.fetched.as_deref(),
				ui.loaded.then_some(ui.total_streams),
			)),
	);

	if ui.loading_catalog && !ui.loaded {
		root = root.child(
			View().class(theme::CARD).child(
				View()
					.class("flex-row items-center gap-3")
					.child(ActivityIndicator(true))
					.child(
						Text("Loading vault streams…")
							.class(theme::BODY)
							.test_id("vault-loading"),
					),
			),
		);
		return root;
	}

	if let Some(error) = &ui.error {
		let retry = state.clone();
		root = root.child(
			View()
				.class(theme::CARD)
				.child(
					Text("Could not load Vault stats")
						.class(theme::HEADING)
						.test_id("vault-error-title"),
				)
				.child(
					Text(error.clone())
						.class(theme::PROBLEM)
						.test_id("vault-error"),
				)
				.child(
					Button("Try again")
						.class(theme::SECONDARY)
						.test_id("vault-retry")
						.on_press(move || retry.reload_vault()),
				),
		);
	}

	if ui.loaded && ui.streams.is_empty() {
		return root.child(
			View()
				.class(theme::CARD)
				.child(
					Text("No streams available")
						.class(theme::HEADING)
						.test_id("vault-empty"),
				)
				.child(
					Text("No streams are shared with this phone for this range.")
						.class(theme::MUTED),
				),
		);
	}

	// Group loaded streams by family, preserving first-seen family order so
	// active families appear cleanly and custom streams appear under their
	// family without being hidden.
	let mut family_order: Vec<String> = Vec::new();
	for stream in &ui.streams {
		if !family_order.contains(&stream.family) {
			family_order.push(stream.family.clone());
		}
	}
	for family in family_order {
		let items: Vec<&StatsStream> = ui
			.streams
			.iter()
			.filter(|stream| stream.family == family)
			.collect();
		root = root.child(family_card(state, &family, &items));
	}

	if let Some(next_offset) = ui.next_offset {
		let more = state.clone();
		let shown = ui.streams.len();
		let total = ui.total_streams;
		root = root.child(
			View()
				.class(theme::CARD)
				.child(
					Text(format!("Showing {shown} of {total} streams"))
						.class(theme::CAPTION)
						.test_id("vault-page-count"),
				)
				.child(
					Button(if ui.loading_more {
						"Loading more streams…"
					} else {
						"Load more streams"
					})
					.class(theme::SECONDARY)
					.test_id("vault-load-more")
					.on_press(move || more.load_more_vault_streams(next_offset)),
				),
		);
	}

	root
}

fn family_card(state: &State, family: &str, streams: &[&StatsStream]) -> Element {
	View()
		.class(theme::CARD)
		.child(
			View()
				.class("flex-row items-center justify-between")
				.child(Text(family.to_owned()).class(theme::HEADING))
				.child(
					Text(if streams.len() == 1 {
						"1 stream".to_owned()
					} else {
						format!("{} streams", streams.len())
					})
					.class(theme::CAPTION),
				),
		)
		.children(streams.iter().map(|stream| stream_row(state, stream)))
}

fn stream_row(state: &State, stream: &StatsStream) -> Element {
	let open = state.clone();
	let stream_id = stream.stream.clone();
	let count_text = format!("{} {}", stream.count, stream.count_unit);
	let last_text = stream.last.as_deref().map_or_else(
		|| "No readings in this range".to_owned(),
		|last| format!("Last {last}"),
	);
	View().class(theme::ROW).child(
		View()
			.class("flex-row items-start justify-between gap-3")
			.child(
				View()
					.class("flex-1 gap-0.5")
					.child(Text(stream.label.clone()).class(theme::LABEL))
					.child(Text(stream.stream.clone()).class(theme::CAPTION))
					.child(Text(last_text).class(theme::MUTED)),
			)
			.child(
				View()
					.class("items-end gap-2")
					.child(Text(count_text).class(if stream.count > 0 {
						theme::BADGE_OK
					} else {
						theme::BADGE_MUTED
					}))
					.child(
						Button("Open")
							.class(theme::COMPACT)
							.accessibility_label(format!("Open {} detail", stream.label))
							.test_id(format!("stream-{}", stream.stream))
							.on_press(move || open.open_vault_stream(stream_id.clone())),
					),
			),
	)
}

fn detail_view(state: &State, ui: &VaultUiState) -> Element {
	let back = state.clone();
	let stream_id = ui.selected_stream.clone().unwrap_or_default();
	let title = ui
		.detail
		.as_ref()
		.map(|s| s.label.clone())
		.or_else(|| {
			ui.streams
				.iter()
				.find(|s| s.stream == stream_id)
				.map(|s| s.label.clone())
		})
		.unwrap_or_else(|| stream_id.clone());

	let mut root = View()
		.class("gap-4")
		.child(
			View().class("flex-row items-center justify-between").child(
				Button("All streams")
					.class(theme::COMPACT)
					.test_id("stream-back")
					.on_press(move || back.close_vault_stream()),
			),
		)
		.child(
			View()
				.class(theme::CARD)
				.child(Text(title).class(theme::TITLE).test_id("detail-title"))
				.child(Text(stream_id).class(theme::CAPTION).test_id("detail-id"))
				.child(range_toolbar(state, ui.window, false))
				.children(range_meta(
					ui.detail_from.as_deref().or(ui.from.as_deref()),
					ui.detail_to.as_deref().or(ui.to.as_deref()),
					ui.detail_fetched.as_deref().or(ui.fetched.as_deref()),
					None,
				)),
		);

	if ui.loading_detail && ui.detail.is_none() {
		return root.child(
			View().class(theme::CARD).child(
				View()
					.class("flex-row items-center gap-3")
					.child(ActivityIndicator(true))
					.child(
						Text("Loading stream detail…")
							.class(theme::BODY)
							.test_id("stream-detail-loading"),
					),
			),
		);
	}

	if let Some(error) = &ui.detail_error {
		let retry = state.clone();
		root = root.child(
			View()
				.class(theme::CARD)
				.child(Text("Could not load stream detail").class(theme::HEADING))
				.child(
					Text(error.clone())
						.class(theme::PROBLEM)
						.test_id("stream-detail-error"),
				)
				.child(
					Button("Try again")
						.class(theme::SECONDARY)
						.test_id("stream-detail-retry")
						.on_press(move || retry.reload_vault()),
				),
		);
	}

	let Some(stream) = &ui.detail else {
		return root;
	};

	let summary_line = format!("{} {} in range", stream.count, stream.count_unit);
	let timing_line = match (&stream.first, &stream.last) {
		(Some(first), Some(last)) if first == last => format!("Observed {last}"),
		(Some(first), Some(last)) => format!("First {first} · Last {last}"),
		(None, Some(last)) => format!("Last {last}"),
		_ => "No readings in this range".to_owned(),
	};
	root = root.child(
		View()
			.class(theme::CARD)
			.child(
				View()
					.class("flex-row items-center justify-between gap-2")
					.child(
						Text(summary_line)
							.class(theme::LABEL)
							.test_id("detail-count"),
					)
					.child(Text(stream.family.clone()).class(theme::BADGE_MUTED)),
			)
			.child(
				Text(timing_line)
					.class(theme::MUTED)
					.test_id("detail-timing"),
			)
			.children(
				(!stream.description.is_empty())
					.then(|| Text(stream.description.clone()).class(theme::CAPTION)),
			),
	);

	match &stream.detail {
		StatsDetail::Numeric {
			channels, buckets, ..
		} => {
			root = root.child(numeric_channels_card(state, ui, channels));
			if !buckets.is_empty() {
				root = root.child(numeric_buckets_card(buckets));
			}
		}
		StatsDetail::Durations {
			group_by, groups, ..
		} => {
			root = root.child(durations_card(state, ui, group_by, groups));
		}
		StatsDetail::Counted => {
			root = root.child(
				View()
					.class(theme::CARD)
					.child(Text("Discrete records").class(theme::HEADING))
					.child(
						Text(format!(
							"This event stream reports record counts ({} {}). Individual record \
							 text stays in your vault.",
							stream.count, stream.count_unit
						))
						.class(theme::MUTED)
						.test_id("detail-counted"),
					),
			);
		}
		StatsDetail::Bounded => {
			let retry = state.clone();
			root = root.child(
				View()
					.class(theme::CARD)
					.child(Text("Detail ready on demand").class(theme::HEADING))
					.child(
						Button("Load detail")
							.class(theme::SECONDARY)
							.test_id("stream-detail-retry")
							.on_press(move || retry.reload_vault()),
					),
			);
		}
	}

	root
}

fn numeric_channels_card(state: &State, ui: &VaultUiState, channels: &[StatsChannel]) -> Element {
	let mut card = View()
		.class(theme::CARD)
		.child(Text("Channels").class(theme::HEADING));

	if channels.is_empty() {
		card =
			card.child(Text("No numeric channels reported for this stream.").class(theme::MUTED));
	} else {
		for channel in channels {
			card = card.child(channel_row(channel));
		}
	}

	if let Some(next_offset) = ui.next_channel_offset {
		let more = state.clone();
		card = card
			.child(
				Text(format!(
					"Showing {} of {} channels",
					channels.len(),
					ui.total_channels
				))
				.class(theme::CAPTION),
			)
			.child(
				Button("Load more channels")
					.class(theme::SECONDARY)
					.test_id("detail-more-channels")
					.on_press(move || more.load_more_stream_channels(next_offset)),
			);
	}
	card
}

fn channel_row(channel: &StatsChannel) -> Element {
	let heading = if channel.unit.is_empty() {
		channel.name.clone()
	} else {
		format!("{} ({})", channel.name, channel.unit)
	};
	let samples = if channel.samples == 1 {
		"1 sample".to_owned()
	} else {
		format!("{} samples", channel.samples)
	};
	let values = match (channel.mean, channel.min, channel.max) {
		(Some(mean), Some(min), Some(max)) if channel.samples > 0 => {
			format!(
				"Mean {} · Min {} · Max {}",
				format_value(mean, &channel.unit),
				format_value(min, &channel.unit),
				format_value(max, &channel.unit),
			)
		}
		_ => "No samples in this range".to_owned(),
	};
	View()
		.class(theme::ROW)
		.child(
			View()
				.class("flex-row items-center justify-between gap-2")
				.child(Text(heading).class(theme::LABEL))
				.child(Text(samples).class(theme::CAPTION)),
		)
		.child(
			Text(values)
				.class(theme::BODY)
				.test_id(format!("channel-{}", channel.name)),
		)
}

fn numeric_buckets_card(buckets: &[StatsBucket]) -> Element {
	let mut card = View()
		.class(theme::CARD)
		.child(Text("Buckets").class(theme::HEADING));
	for bucket in buckets {
		let summary = bucket
			.channels
			.iter()
			.map(|ch| {
				format!(
					"{}: {} ({} samples)",
					ch.name,
					format_value(ch.mean, &ch.unit),
					ch.samples
				)
			})
			.collect::<Vec<_>>()
			.join(" · ");
		card = card.child(
			View()
				.class(theme::ROW)
				.child(Text(bucket.start.clone()).class(theme::CAPTION))
				.child(Text(summary).class(theme::MUTED)),
		);
	}
	card
}

fn durations_card(
	state: &State,
	ui: &VaultUiState,
	group_by: &str,
	groups: &[StatsDuration],
) -> Element {
	let mut card = View()
		.class(theme::CARD)
		.child(Text(format!("Time by {group_by}")).class(theme::HEADING));

	if groups.is_empty() {
		card = card.child(Text("No duration groups in this range.").class(theme::MUTED));
	} else {
		for group in groups {
			let name = group.group.as_deref().unwrap_or("Other");
			let records = if group.count == 1 {
				"1 record".to_owned()
			} else {
				format!("{} records", group.count)
			};
			card = card.child(
				View()
					.class(theme::ROW)
					.child(
						View()
							.class("flex-row items-center justify-between gap-2")
							.child(Text(name.to_owned()).class(theme::LABEL))
							.child(Text(group.duration.clone()).class(theme::BADGE_OK)),
					)
					.child(Text(records).class(theme::CAPTION)),
			);
		}
	}

	if let Some(next_offset) = ui.next_group_offset {
		let more = state.clone();
		card = card
			.child(
				Text(format!(
					"Showing {} of {} groups",
					groups.len(),
					ui.total_groups
				))
				.class(theme::CAPTION),
			)
			.child(
				Button("Load more groups")
					.class(theme::SECONDARY)
					.test_id("detail-more-groups")
					.on_press(move || more.load_more_stream_groups(next_offset)),
			);
	}
	card
}

fn range_meta(
	from: Option<&str>,
	to: Option<&str>,
	fetched: Option<&str>,
	total_streams: Option<usize>,
) -> Option<Element> {
	let range = match (from, to) {
		(Some(from), Some(to)) => Some(format!("Range: {from} – {to}")),
		_ => None,
	};
	let mut parts = Vec::new();
	if let Some(total) = total_streams {
		parts.push(if total == 1 {
			"1 stream".to_owned()
		} else {
			format!("{total} streams")
		});
	}
	if let Some(fetched) = fetched {
		parts.push(format!("Updated {fetched}"));
	}
	let tail = (!parts.is_empty()).then(|| parts.join(" · "));
	if range.is_none() && tail.is_none() {
		return None;
	}
	Some(
		View()
			.class("gap-1 pt-2 border-t border-[#2A2560]")
			.children(range.map(|text| {
				Text(text)
					.class(theme::MUTED)
					.test_id("vault-effective-range")
			}))
			.children(tail.map(|text| Text(text).class(theme::CAPTION))),
	)
}

/// Format a numeric aggregate with its exact declared unit (never inventing a
/// unit when `unit` is empty).
pub(crate) fn format_value(value: f64, unit: &str) -> String {
	let number = if !value.is_finite() {
		"—".to_owned()
	} else if (value - value.round()).abs() < 0.05 {
		format!("{value:.0}")
	} else {
		format!("{value:.1}")
	};
	if unit.is_empty() {
		number
	} else {
		format!("{number} {unit}")
	}
}
