//! The bounded, signed summary the opt-in stats companion reads from the
//! owner's vault.
//!
//! Nothing here touches the spool, the collector's uploader, or the local
//! write queue. A summary is built on demand from four read routes only:
//! the stream catalog, the overview, series buckets, and duration totals.

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

/// Read a bounded summary of the owner's vault through `client`.
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
	// Streams with data come first, so the detail budget reaches the data.
	// Everything else follows in a stable family order, including streams
	// that are empty in this range.
	seeds.sort_by_key(|seed| {
		(
			seed.count == 0,
			family_rank(seed.spec.id.family()),
			seed.spec.id.to_string(),
		)
	});
	let truncated = seeds.len() > MAX_STATS_STREAMS;
	seeds.truncate(MAX_STATS_STREAMS);

	let span_seconds = overview.to.duration_since(overview.from).as_secs().max(1);
	let bucket_count = u64::try_from(MAX_STATS_BUCKETS).unwrap_or(1);
	let bucket_seconds = u64::try_from(span_seconds)
		.unwrap_or(u64::MAX)
		.div_ceil(bucket_count)
		.max(60);

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
		streams.push(StatsStream {
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
		});
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
		fetched: words::when(now, now, tz),
	})
}

struct Seed {
	spec: StreamSpec,
	count: u64,
	first: Option<Timestamp>,
	last: Option<Timestamp>,
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
				return Ok(empty_numeric(&seed.spec));
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
			Ok(numeric_detail(&seed.spec, &series.buckets, now, tz))
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
			let mut groups: BTreeMap<Option<String>, (u64, f64)> = BTreeMap::new();
			for item in durations.items {
				let entry = groups.entry(item.group).or_default();
				entry.0 = entry.0.saturating_add(item.count);
				entry.1 += item.seconds;
			}
			let mut groups: Vec<StatsDuration> = groups
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
			groups.sort_by(|left, right| {
				right
					.seconds
					.total_cmp(&left.seconds)
					.then_with(|| left.group.cmp(&right.group))
			});
			let truncated = groups.len() > MAX_STATS_GROUPS;
			groups.truncate(MAX_STATS_GROUPS);
			Ok(StatsDetail::Durations {
				group_by: group_by.clone(),
				groups,
				truncated,
			})
		}
	}
}

fn empty_numeric(spec: &StreamSpec) -> StatsDetail {
	StatsDetail::Numeric {
		channels: spec
			.channels
			.iter()
			.take(MAX_STATS_CHANNELS)
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
			.collect(),
		buckets: Vec::new(),
		channels_truncated: spec.channels.len() > MAX_STATS_CHANNELS,
		buckets_truncated: false,
	}
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

fn numeric_detail(
	spec: &StreamSpec,
	buckets: &[Bucket],
	now: Timestamp,
	tz: &TimeZone,
) -> StatsDetail {
	let mut totals: BTreeMap<String, Total> = BTreeMap::new();
	for bucket in buckets {
		for (name, summary) in &bucket.channels {
			totals.entry(name.clone()).or_default().add(summary);
		}
	}
	// Known channels keep the spec's order and unit; anything the vault
	// added later follows by name.
	let mut channels = Vec::new();
	for channel in &spec.channels {
		if channels.len() >= MAX_STATS_CHANNELS {
			break;
		}
		channels.push(channel_row(channel, totals.get(&channel.name)));
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
		if channels.len() >= MAX_STATS_CHANNELS {
			break;
		}
		channels.push(StatsChannel {
			name: name.clone(),
			unit: String::new(),
			samples: totals.get(name).map_or(0, |total| total.samples),
			min: totals.get(name).map(|total| total.min),
			max: totals.get(name).map(|total| total.max),
			mean: totals.get(name).and_then(Total::mean),
		});
	}
	let channels_truncated = spec.channels.len().max(totals.len()) > MAX_STATS_CHANNELS;

	let buckets_truncated = buckets.len() > MAX_STATS_BUCKETS;
	let buckets: Vec<StatsBucket> = buckets
		.iter()
		.take(MAX_STATS_BUCKETS)
		.map(|bucket| {
			StatsBucket {
				start: words::when(bucket.start, now, tz),
				start_ms: bucket.start.as_millisecond(),
				channels: bucket_rows(spec, &bucket.channels),
			}
		})
		.collect();

	StatsDetail::Numeric {
		channels,
		buckets,
		channels_truncated,
		buckets_truncated,
	}
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

fn bucket_rows(
	spec: &StreamSpec,
	channels: &BTreeMap<String, ChannelSummary>,
) -> Vec<StatsBucketChannel> {
	let mut rows = Vec::new();
	for channel in &spec.channels {
		if rows.len() >= MAX_STATS_CHANNELS {
			break;
		}
		if let Some(summary) = channels.get(&channel.name) {
			rows.push(bucket_row(&channel.name, &channel.unit, summary));
		}
	}
	let mut unknown: Vec<&String> = channels
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
		if rows.len() >= MAX_STATS_CHANNELS {
			break;
		}
		if let Some(summary) = channels.get(name) {
			rows.push(bucket_row(name, "", summary));
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
