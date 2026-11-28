//! The bounded, signed summary, paged stream catalog, and on-demand stream
//! detail the opt-in stats companion reads from the owner's vault.
//!
//! Nothing here touches the spool, the collector's uploader, or the local
//! write queue. Reads use four summary-capable routes only: the stream
//! catalog, the overview, series buckets, and duration totals.

use std::collections::BTreeMap;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_api::Bucket;
use quant_api::ChannelSummary;
use quant_api::DurationsRequest;
use quant_api::OverviewItem;
use quant_api::OverviewRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::ClientError;
use quant_core::Channel;
use quant_core::Family;
use quant_core::StreamKind;
use quant_core::StreamSpec;
use quant_model::phone::MAX_STATS_BUCKETS;
use quant_model::phone::MAX_STATS_CHANNELS;
use quant_model::phone::MAX_STATS_DETAILS;
use quant_model::phone::MAX_STATS_GROUPS;
use quant_model::phone::MAX_STATS_STREAMS;
use quant_model::phone::StatsBucket;
use quant_model::phone::StatsBucketChannel;
use quant_model::phone::StatsChannel;
use quant_model::phone::StatsDetail;
use quant_model::phone::StatsDuration;
use quant_model::phone::StatsKind;
use quant_model::phone::StatsStream;
use quant_model::phone::StatsWindow;
use quant_model::phone::VaultStats;

use crate::words;

/// One stream's on-demand detail with bounded channel and duration-group
/// paging metadata.
#[derive(Clone, Debug)]
pub struct StreamDetailPage {
	/// Effective range start, display-formatted.
	pub from: String,
	/// Effective range end, display-formatted.
	pub to: String,
	/// Effective range start, Unix milliseconds.
	pub from_ms: i64,
	/// Effective range end, Unix milliseconds.
	pub to_ms: i64,
	/// When the vault answered, display-formatted.
	pub fetched: String,
	/// The stream and its loaded detail slice.
	pub stream: StatsStream,
	/// Starting offset of returned numeric channels.
	pub channel_offset: usize,
	/// Total channels on this stream.
	pub total_channels: usize,
	/// Next channel offset, when more channels exist.
	pub next_channel_offset: Option<usize>,
	/// Starting offset of returned duration groups.
	pub group_offset: usize,
	/// Total duration groups on this stream.
	pub total_groups: usize,
	/// Next duration-group offset, when more groups exist.
	pub next_group_offset: Option<usize>,
}

/// Read a bounded first-page summary of the owner's vault through `client`,
/// including inline details for up to [`MAX_STATS_DETAILS`] active streams.
///
/// The effective range is the one the vault reports after clipping the
/// request to the companion grant's lookback. An empty stream is listed with
/// no data; a failed read returns an error and no summary at all.
pub async fn summary(
	client: &Client,
	window: StatsWindow,
	tz: &TimeZone,
) -> Result<VaultStats, ClientError> {
	let now = Timestamp::now();
	let (overview, mut seeds) = load_catalog(client, window, now).await?;
	let total_streams = seeds.len();
	let truncated = total_streams > MAX_STATS_STREAMS;
	seeds.truncate(MAX_STATS_STREAMS);
	let next_offset = (seeds.len() < total_streams).then_some(seeds.len());

	let bucket_seconds = bucket_width(&overview);
	let mut detail_budget = MAX_STATS_DETAILS;
	let mut details_truncated = false;
	let mut streams = Vec::with_capacity(seeds.len());
	for seed in seeds {
		let detail = detail_for(
			client,
			&seed,
			&overview,
			bucket_seconds,
			&mut detail_budget,
			now,
			tz,
		)
		.await?;
		if matches!(&detail, StatsDetail::Bounded) {
			details_truncated = true;
		}
		streams.push(build_stream(&seed, detail, now, tz));
	}

	Ok(VaultStats {
		window,
		span: window.label().to_owned(),
		from: words::when(overview.from, now, tz),
		to: words::when(overview.to, now, tz),
		from_ms: overview.from.as_millisecond(),
		to_ms: overview.to.as_millisecond(),
		streams,
		truncated,
		details_truncated,
		offset: 0,
		total_streams,
		next_offset,
		fetched: words::when(now, now, tz),
	})
}

/// Read one bounded page of every permitted stream in the vault catalog,
/// including custom streams and streams with zero readings in `window`.
///
/// Detail for non-empty numeric and duration streams is left as
/// [`StatsDetail::Bounded`] so browsing many pages stays fast; opening any
/// stream fetches its detail on demand through [`stream_detail`].
pub async fn catalog_page(
	client: &Client,
	window: StatsWindow,
	offset: usize,
	limit: usize,
	tz: &TimeZone,
) -> Result<VaultStats, ClientError> {
	let now = Timestamp::now();
	let (overview, seeds) = load_catalog(client, window, now).await?;
	let total_streams = seeds.len();
	let page_size = limit.clamp(1, MAX_STATS_STREAMS);
	let start = offset.min(total_streams);
	let end = start.saturating_add(page_size).min(total_streams);
	let next_offset = (end < total_streams).then_some(end);

	let mut details_truncated = false;
	let mut streams = Vec::with_capacity(end.saturating_sub(start));
	for seed in &seeds[start..end] {
		let detail = catalog_detail(seed);
		if matches!(&detail, StatsDetail::Bounded) {
			details_truncated = true;
		}
		streams.push(build_stream(seed, detail, now, tz));
	}

	Ok(VaultStats {
		window,
		span: window.label().to_owned(),
		from: words::when(overview.from, now, tz),
		to: words::when(overview.to, now, tz),
		from_ms: overview.from.as_millisecond(),
		to_ms: overview.to.as_millisecond(),
		streams,
		truncated: next_offset.is_some(),
		details_truncated,
		offset: start,
		total_streams,
		next_offset,
		fetched: words::when(now, now, tz),
	})
}

/// Read on-demand detail for any single permitted stream, with bounded paging
/// over its numeric channels or duration groups so no channel or group is
/// silently hidden.
pub async fn stream_detail(
	client: &Client,
	stream_id: &str,
	window: StatsWindow,
	channel_offset: usize,
	group_offset: usize,
	tz: &TimeZone,
) -> Result<Option<StreamDetailPage>, ClientError> {
	let now = Timestamp::now();
	let from = now
		.checked_sub(SignedDuration::from_millis(window.span_ms()))
		.unwrap_or(Timestamp::MIN);
	let catalog = client.streams().await?;
	let Some(access) = catalog
		.into_iter()
		.find(|access| access.spec.id.as_str() == stream_id)
	else {
		return Ok(None);
	};
	let overview = client
		.overview(
			&OverviewRequest {
				from: Some(from),
				to: Some(now),
			},
			None,
		)
		.await?;
	let item = overview
		.streams
		.iter()
		.find(|item| item.stream.as_str() == stream_id);
	let seed = Seed {
		spec: access.spec,
		count: item.map_or(0, |item| item.count),
		first: item.map(|item| item.first),
		last: item.map(|item| item.last),
	};
	let bucket_seconds = bucket_width(&overview);

	let (detail, channel_page, group_page) = match seed.spec.kind {
		StreamKind::Series => {
			if seed.count == 0 {
				let (detail, page) = empty_numeric_page(&seed.spec, channel_offset);
				(detail, page, PageMeta::empty())
			} else {
				let series = client
					.series(
						&SeriesRequest {
							stream: seed.spec.id.to_string(),
							from: Some(overview.from),
							to: Some(overview.to),
							bucket_seconds: Some(bucket_seconds),
							raw: false,
							max_rows: None,
						},
						None,
					)
					.await?;
				let (detail, page) =
					numeric_detail_page(&seed.spec, &series.buckets, channel_offset, now, tz);
				(detail, page, PageMeta::empty())
			}
		}
		StreamKind::Event => {
			let Some(group_by) = seed.spec.summary_keys.first() else {
				return Ok(Some(StreamDetailPage {
					from: words::when(overview.from, now, tz),
					to: words::when(overview.to, now, tz),
					from_ms: overview.from.as_millisecond(),
					to_ms: overview.to.as_millisecond(),
					fetched: words::when(now, now, tz),
					stream: build_stream(&seed, StatsDetail::Counted, now, tz),
					channel_offset: 0,
					total_channels: 0,
					next_channel_offset: None,
					group_offset: 0,
					total_groups: 0,
					next_group_offset: None,
				}));
			};
			if seed.count == 0 {
				(
					StatsDetail::Durations {
						group_by: group_by.clone(),
						groups: Vec::new(),
						truncated: false,
					},
					PageMeta::empty(),
					PageMeta::empty(),
				)
			} else {
				let durations = client
					.durations(
						&DurationsRequest {
							stream: seed.spec.id.to_string(),
							from: Some(overview.from),
							to: Some(overview.to),
							group_by: Some(group_by.clone()),
							bucket_seconds: None,
						},
						None,
					)
					.await?;
				let (detail, page) =
					durations_detail_page(group_by, &durations.items, group_offset);
				(detail, PageMeta::empty(), page)
			}
		}
	};

	Ok(Some(StreamDetailPage {
		from: words::when(overview.from, now, tz),
		to: words::when(overview.to, now, tz),
		from_ms: overview.from.as_millisecond(),
		to_ms: overview.to.as_millisecond(),
		fetched: words::when(now, now, tz),
		stream: build_stream(&seed, detail, now, tz),
		channel_offset: channel_page.offset,
		total_channels: channel_page.total,
		next_channel_offset: channel_page.next_offset,
		group_offset: group_page.offset,
		total_groups: group_page.total,
		next_group_offset: group_page.next_offset,
	}))
}

#[derive(Clone, Copy, Debug)]
struct PageMeta {
	offset: usize,
	total: usize,
	next_offset: Option<usize>,
}

impl PageMeta {
	const fn empty() -> Self {
		Self {
			offset: 0,
			total: 0,
			next_offset: None,
		}
	}
}

struct Seed {
	spec: StreamSpec,
	count: u64,
	first: Option<Timestamp>,
	last: Option<Timestamp>,
}

async fn load_catalog(
	client: &Client,
	window: StatsWindow,
	now: Timestamp,
) -> Result<(quant_api::Overview, Vec<Seed>), ClientError> {
	let from = now
		.checked_sub(SignedDuration::from_millis(window.span_ms()))
		.unwrap_or(Timestamp::MIN);
	let catalog = client.streams().await?;
	let overview = client
		.overview(
			&OverviewRequest {
				from: Some(from),
				to: Some(now),
			},
			None,
		)
		.await?;

	let mut counts: BTreeMap<String, OverviewItem> = overview
		.streams
		.iter()
		.map(|item| (item.stream.to_string(), item.clone()))
		.collect();
	let mut seeds: Vec<Seed> = catalog
		.into_iter()
		.map(|access| {
			let count = counts.remove(access.spec.id.as_str());
			Seed {
				spec: access.spec,
				count: count.as_ref().map_or(0, |item| item.count),
				first: count.as_ref().map(|item| item.first),
				last: count.as_ref().map(|item| item.last),
			}
		})
		.collect();
	// Streams with data come first, then everything else in a stable family
	// order, including streams that are empty in this range and custom streams.
	seeds.sort_by_key(|seed| {
		(
			seed.count == 0,
			family_rank(seed.spec.id.family()),
			seed.spec.id.to_string(),
		)
	});
	Ok((overview, seeds))
}

fn bucket_width(overview: &quant_api::Overview) -> u64 {
	let span_seconds = overview.to.duration_since(overview.from).as_secs().max(1);
	let bucket_count = u64::try_from(MAX_STATS_BUCKETS).unwrap_or(1);
	u64::try_from(span_seconds)
		.unwrap_or(u64::MAX)
		.div_ceil(bucket_count)
		.max(60)
}

fn build_stream(seed: &Seed, detail: StatsDetail, now: Timestamp, tz: &TimeZone) -> StatsStream {
	StatsStream {
		stream: seed.spec.id.to_string(),
		label: words::stream_label(seed.spec.id.as_str()),
		family: words::family_label(seed.spec.id.family()).to_owned(),
		description: words::shorten(&seed.spec.description, 180),
		kind: match seed.spec.kind {
			StreamKind::Event => StatsKind::Event,
			StreamKind::Series => StatsKind::Series,
		},
		count: seed.count,
		count_unit: match seed.spec.kind {
			StreamKind::Event => "records",
			StreamKind::Series => "samples",
		}
		.to_owned(),
		first: seed.first.map(|at| words::when(at, now, tz)),
		last: seed.last.map(|at| words::when(at, now, tz)),
		first_ms: seed.first.map(Timestamp::as_millisecond),
		last_ms: seed.last.map(Timestamp::as_millisecond),
		detail,
	}
}

fn catalog_detail(seed: &Seed) -> StatsDetail {
	match seed.spec.kind {
		StreamKind::Series => {
			if seed.count == 0 {
				empty_numeric_page(&seed.spec, 0).0
			} else {
				StatsDetail::Bounded
			}
		}
		StreamKind::Event => {
			let Some(group_by) = seed.spec.summary_keys.first() else {
				return StatsDetail::Counted;
			};
			if seed.count == 0 {
				StatsDetail::Durations {
					group_by: group_by.clone(),
					groups: Vec::new(),
					truncated: false,
				}
			} else {
				StatsDetail::Bounded
			}
		}
	}
}

async fn detail_for(
	client: &Client,
	seed: &Seed,
	overview: &quant_api::Overview,
	bucket_seconds: u64,
	detail_budget: &mut usize,
	now: Timestamp,
	tz: &TimeZone,
) -> Result<StatsDetail, ClientError> {
	match seed.spec.kind {
		StreamKind::Series => {
			if seed.count == 0 {
				return Ok(empty_numeric_page(&seed.spec, 0).0);
			}
			if *detail_budget == 0 {
				return Ok(StatsDetail::Bounded);
			}
			*detail_budget -= 1;
			let series = client
				.series(
					&SeriesRequest {
						stream: seed.spec.id.to_string(),
						from: Some(overview.from),
						to: Some(overview.to),
						bucket_seconds: Some(bucket_seconds),
						raw: false,
						max_rows: None,
					},
					None,
				)
				.await?;
			Ok(numeric_detail_page(&seed.spec, &series.buckets, 0, now, tz).0)
		}
		StreamKind::Event => {
			let Some(group_by) = seed.spec.summary_keys.first() else {
				return Ok(StatsDetail::Counted);
			};
			if seed.count == 0 {
				return Ok(StatsDetail::Durations {
					group_by: group_by.clone(),
					groups: Vec::new(),
					truncated: false,
				});
			}
			if *detail_budget == 0 {
				return Ok(StatsDetail::Bounded);
			}
			*detail_budget -= 1;
			let durations = client
				.durations(
					&DurationsRequest {
						stream: seed.spec.id.to_string(),
						from: Some(overview.from),
						to: Some(overview.to),
						group_by: Some(group_by.clone()),
						bucket_seconds: None,
					},
					None,
				)
				.await?;
			Ok(durations_detail_page(group_by, &durations.items, 0).0)
		}
	}
}

fn durations_detail_page(
	group_by: &str,
	items: &[quant_api::DurationItem],
	offset: usize,
) -> (StatsDetail, PageMeta) {
	let mut totals: BTreeMap<Option<String>, (u64, f64)> = BTreeMap::new();
	for item in items {
		let entry = totals.entry(item.group.clone()).or_default();
		entry.0 = entry.0.saturating_add(item.count);
		entry.1 += item.seconds;
	}
	let mut all: Vec<StatsDuration> = totals
		.into_iter()
		.map(|(group, (count, seconds))| {
			StatsDuration {
				group: group.map(|group| words::shorten(&group, 80)),
				count,
				seconds,
				duration: words::duration(seconds),
			}
		})
		.collect();
	all.sort_by(|left, right| {
		right
			.seconds
			.total_cmp(&left.seconds)
			.then_with(|| left.group.cmp(&right.group))
	});
	let total = all.len();
	let start = offset.min(total);
	let end = start.saturating_add(MAX_STATS_GROUPS).min(total);
	let next_offset = (end < total).then_some(end);
	let groups = all[start..end].to_vec();
	(
		StatsDetail::Durations {
			group_by: group_by.to_owned(),
			groups,
			truncated: next_offset.is_some(),
		},
		PageMeta {
			offset: start,
			total,
			next_offset,
		},
	)
}

fn empty_numeric_page(spec: &StreamSpec, offset: usize) -> (StatsDetail, PageMeta) {
	let total = spec.channels.len();
	let start = offset.min(total);
	let end = start.saturating_add(MAX_STATS_CHANNELS).min(total);
	let next_offset = (end < total).then_some(end);
	let channels = spec.channels[start..end]
		.iter()
		.map(|channel| {
			StatsChannel {
				name: channel.name.clone(),
				unit: channel.unit.clone(),
				samples: 0,
				min: None,
				max: None,
				mean: None,
			}
		})
		.collect();
	(
		StatsDetail::Numeric {
			channels,
			buckets: Vec::new(),
			channels_truncated: next_offset.is_some(),
			buckets_truncated: false,
		},
		PageMeta {
			offset: start,
			total,
			next_offset,
		},
	)
}

#[derive(Default)]
struct Total {
	samples: u64,
	min: f64,
	max: f64,
	sum: f64,
}

impl Total {
	fn add(&mut self, summary: &ChannelSummary) {
		if self.samples == 0 {
			self.min = summary.min;
			self.max = summary.max;
		} else {
			self.min = self.min.min(summary.min);
			self.max = self.max.max(summary.max);
		}
		self.samples = self.samples.saturating_add(summary.count);
		#[allow(clippy::cast_precision_loss)]
		let count = summary.count as f64;
		self.sum += summary.mean * count;
	}

	fn mean(&self) -> Option<f64> {
		(self.samples > 0).then(|| {
			#[allow(clippy::cast_precision_loss)]
			let samples = self.samples as f64;
			self.sum / samples
		})
	}
}

fn numeric_detail_page(
	spec: &StreamSpec,
	buckets: &[Bucket],
	channel_offset: usize,
	now: Timestamp,
	tz: &TimeZone,
) -> (StatsDetail, PageMeta) {
	let mut totals: BTreeMap<String, Total> = BTreeMap::new();
	for bucket in buckets {
		for (name, summary) in &bucket.channels {
			totals.entry(name.clone()).or_default().add(summary);
		}
	}
	// Known channels keep the spec's order and exact unit; anything the vault
	// added later follows by name with an empty unit (never guessed).
	let mut all_channels = Vec::with_capacity(spec.channels.len().max(totals.len()));
	for channel in &spec.channels {
		all_channels.push(channel_row(channel, totals.get(&channel.name)));
	}
	let mut unknown: Vec<&String> = totals
		.keys()
		.filter(|name| {
			!spec
				.channels
				.iter()
				.any(|channel| channel.name.as_str() == name.as_str())
		})
		.collect();
	unknown.sort();
	for name in unknown {
		all_channels.push(StatsChannel {
			name: name.clone(),
			unit: String::new(),
			samples: totals.get(name).map_or(0, |total| total.samples),
			min: totals.get(name).map(|total| total.min),
			max: totals.get(name).map(|total| total.max),
			mean: totals.get(name).and_then(Total::mean),
		});
	}

	let total_channels = all_channels.len();
	let start = channel_offset.min(total_channels);
	let end = start.saturating_add(MAX_STATS_CHANNELS).min(total_channels);
	let next_offset = (end < total_channels).then_some(end);
	let channels = all_channels[start..end].to_vec();

	// Keep the latest bounded buckets (a range straddling a bucket boundary
	// can produce MAX_STATS_BUCKETS + 1 buckets, so keep the tail up to +2
	// rather than dropping the newest bucket).
	let bucket_cap = MAX_STATS_BUCKETS.saturating_add(2);
	let buckets_truncated = buckets.len() > bucket_cap;
	let bucket_start = buckets.len().saturating_sub(bucket_cap);
	let buckets: Vec<StatsBucket> = buckets[bucket_start..]
		.iter()
		.map(|bucket| {
			StatsBucket {
				start: words::when(bucket.start, now, tz),
				start_ms: bucket.start.as_millisecond(),
				channels: bucket_rows_for(&channels, &bucket.channels),
			}
		})
		.collect();

	(
		StatsDetail::Numeric {
			channels,
			buckets,
			channels_truncated: next_offset.is_some(),
			buckets_truncated,
		},
		PageMeta {
			offset: start,
			total: total_channels,
			next_offset,
		},
	)
}

fn channel_row(channel: &Channel, total: Option<&Total>) -> StatsChannel {
	match total {
		Some(total) => {
			StatsChannel {
				name: channel.name.clone(),
				unit: channel.unit.clone(),
				samples: total.samples,
				min: Some(total.min),
				max: Some(total.max),
				mean: total.mean(),
			}
		}
		None => {
			StatsChannel {
				name: channel.name.clone(),
				unit: channel.unit.clone(),
				samples: 0,
				min: None,
				max: None,
				mean: None,
			}
		}
	}
}

fn bucket_rows_for(
	page_channels: &[StatsChannel],
	channels: &BTreeMap<String, ChannelSummary>,
) -> Vec<StatsBucketChannel> {
	let mut rows = Vec::new();
	for channel in page_channels {
		if let Some(summary) = channels.get(&channel.name) {
			rows.push(bucket_row(&channel.name, &channel.unit, summary));
		}
	}
	rows
}

fn bucket_row(name: &str, unit: &str, summary: &ChannelSummary) -> StatsBucketChannel {
	StatsBucketChannel {
		name: name.to_owned(),
		unit: unit.to_owned(),
		samples: summary.count,
		min: summary.min,
		max: summary.max,
		mean: summary.mean,
	}
}

fn family_rank(family: Family) -> usize {
	Family::ALL
		.iter()
		.position(|candidate| *candidate == family)
		.unwrap_or(usize::MAX)
}
