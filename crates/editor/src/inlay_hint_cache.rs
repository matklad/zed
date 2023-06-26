use std::{
    cmp,
    ops::{ControlFlow, Range},
    sync::Arc,
};

use crate::{
    display_map::Inlay, Anchor, Editor, ExcerptId, InlayId, MultiBuffer, MultiBufferSnapshot,
};
use anyhow::Context;
use clock::Global;
use gpui::{ModelHandle, Task, ViewContext};
use language::{language_settings::InlayHintKind, Buffer, BufferSnapshot};
use log::error;
use parking_lot::RwLock;
use project::InlayHint;

use collections::{hash_map, HashMap, HashSet};
use language::language_settings::InlayHintSettings;
use util::post_inc;

pub struct InlayHintCache {
    pub hints: HashMap<ExcerptId, Arc<RwLock<CachedExcerptHints>>>,
    pub allowed_hint_kinds: HashSet<Option<InlayHintKind>>,
    pub version: usize,
    pub enabled: bool,
    update_tasks: HashMap<ExcerptId, UpdateTask>,
}

struct UpdateTask {
    current: (InvalidationStrategy, SpawnedTask),
    pending_refresh: Option<SpawnedTask>,
}

struct SpawnedTask {
    version: usize,
    is_running_rx: smol::channel::Receiver<()>,
    _task: Task<()>,
}

#[derive(Debug)]
pub struct CachedExcerptHints {
    version: usize,
    buffer_version: Global,
    pub hints: Vec<(InlayId, InlayHint)>,
}

#[derive(Debug, Clone, Copy)]
struct ExcerptQuery {
    buffer_id: u64,
    excerpt_id: ExcerptId,
    dimensions: ExcerptDimensions,
    cache_version: usize,
    invalidate: InvalidationStrategy,
}

#[derive(Debug, Clone, Copy)]
struct ExcerptDimensions {
    excerpt_range_start: language::Anchor,
    excerpt_range_end: language::Anchor,
    excerpt_visible_range_start: language::Anchor,
    excerpt_visible_range_end: language::Anchor,
}

impl ExcerptQuery {
    fn hints_fetch_ranges(&self, buffer: &BufferSnapshot) -> HintFetchRanges {
        let visible_range =
            self.dimensions.excerpt_visible_range_start..self.dimensions.excerpt_visible_range_end;
        let mut other_ranges = Vec::new();
        if self
            .dimensions
            .excerpt_range_start
            .cmp(&self.dimensions.excerpt_visible_range_start, buffer)
            .is_lt()
        {
            let mut end = self.dimensions.excerpt_visible_range_start;
            end.offset -= 1;
            other_ranges.push(self.dimensions.excerpt_range_start..end);
        }
        if self
            .dimensions
            .excerpt_range_end
            .cmp(&self.dimensions.excerpt_visible_range_end, buffer)
            .is_gt()
        {
            let mut start = self.dimensions.excerpt_visible_range_end;
            start.offset += 1;
            other_ranges.push(start..self.dimensions.excerpt_range_end);
        }

        HintFetchRanges {
            visible_range,
            other_ranges: other_ranges.into_iter().map(|range| range).collect(),
        }
    }
}

impl UpdateTask {
    fn new(invalidation_strategy: InvalidationStrategy, spawned_task: SpawnedTask) -> Self {
        Self {
            current: (invalidation_strategy, spawned_task),
            pending_refresh: None,
        }
    }

    fn is_running(&self) -> bool {
        !self.current.1.is_running_rx.is_closed()
            || self
                .pending_refresh
                .as_ref()
                .map_or(false, |task| !task.is_running_rx.is_closed())
    }

    fn cache_version(&self) -> usize {
        self.current.1.version
    }

    fn invalidation_strategy(&self) -> InvalidationStrategy {
        self.current.0
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InvalidationStrategy {
    Forced,
    OnConflict,
    None,
}

#[derive(Debug, Default)]
pub struct InlaySplice {
    pub to_remove: Vec<InlayId>,
    pub to_insert: Vec<(Anchor, InlayId, InlayHint)>,
}

#[derive(Debug)]
struct ExcerptHintsUpdate {
    excerpt_id: ExcerptId,
    cache_version: usize,
    remove_from_visible: Vec<InlayId>,
    remove_from_cache: HashSet<InlayId>,
    add_to_cache: HashSet<InlayHint>,
}

impl InlayHintCache {
    pub fn new(inlay_hint_settings: InlayHintSettings) -> Self {
        Self {
            allowed_hint_kinds: inlay_hint_settings.enabled_inlay_hint_kinds(),
            enabled: inlay_hint_settings.enabled,
            hints: HashMap::default(),
            update_tasks: HashMap::default(),
            version: 0,
        }
    }

    pub fn update_settings(
        &mut self,
        multi_buffer: &ModelHandle<MultiBuffer>,
        new_hint_settings: InlayHintSettings,
        visible_hints: Vec<Inlay>,
        cx: &mut ViewContext<Editor>,
    ) -> ControlFlow<Option<InlaySplice>> {
        let new_allowed_hint_kinds = new_hint_settings.enabled_inlay_hint_kinds();
        match (self.enabled, new_hint_settings.enabled) {
            (false, false) => {
                self.allowed_hint_kinds = new_allowed_hint_kinds;
                ControlFlow::Break(None)
            }
            (true, true) => {
                if new_allowed_hint_kinds == self.allowed_hint_kinds {
                    ControlFlow::Break(None)
                } else {
                    let new_splice = self.new_allowed_hint_kinds_splice(
                        multi_buffer,
                        &visible_hints,
                        &new_allowed_hint_kinds,
                        cx,
                    );
                    if new_splice.is_some() {
                        self.version += 1;
                        self.update_tasks.clear();
                        self.allowed_hint_kinds = new_allowed_hint_kinds;
                    }
                    ControlFlow::Break(new_splice)
                }
            }
            (true, false) => {
                self.enabled = new_hint_settings.enabled;
                self.allowed_hint_kinds = new_allowed_hint_kinds;
                if self.hints.is_empty() {
                    ControlFlow::Break(None)
                } else {
                    self.clear();
                    ControlFlow::Break(Some(InlaySplice {
                        to_remove: visible_hints.iter().map(|inlay| inlay.id).collect(),
                        to_insert: Vec::new(),
                    }))
                }
            }
            (false, true) => {
                self.enabled = new_hint_settings.enabled;
                self.allowed_hint_kinds = new_allowed_hint_kinds;
                ControlFlow::Continue(())
            }
        }
    }

    pub fn refresh_inlay_hints(
        &mut self,
        mut excerpts_to_query: HashMap<ExcerptId, (ModelHandle<Buffer>, Range<usize>)>,
        invalidate: InvalidationStrategy,
        cx: &mut ViewContext<Editor>,
    ) {
        if !self.enabled {
            return;
        }
        let update_tasks = &mut self.update_tasks;
        let invalidate_cache = matches!(
            invalidate,
            InvalidationStrategy::Forced | InvalidationStrategy::OnConflict
        );
        if invalidate_cache {
            update_tasks
                .retain(|task_excerpt_id, _| excerpts_to_query.contains_key(task_excerpt_id));
        }
        let cache_version = self.version;
        excerpts_to_query.retain(|visible_excerpt_id, _| {
            match update_tasks.entry(*visible_excerpt_id) {
                hash_map::Entry::Occupied(o) => match o.get().cache_version().cmp(&cache_version) {
                    cmp::Ordering::Less => true,
                    cmp::Ordering::Equal => invalidate_cache,
                    cmp::Ordering::Greater => false,
                },
                hash_map::Entry::Vacant(_) => true,
            }
        });

        cx.spawn(|editor, mut cx| async move {
            editor
                .update(&mut cx, |editor, cx| {
                    spawn_new_update_tasks(editor, excerpts_to_query, invalidate, cache_version, cx)
                })
                .ok();
        })
        .detach();
    }

    fn new_allowed_hint_kinds_splice(
        &self,
        multi_buffer: &ModelHandle<MultiBuffer>,
        visible_hints: &[Inlay],
        new_kinds: &HashSet<Option<InlayHintKind>>,
        cx: &mut ViewContext<Editor>,
    ) -> Option<InlaySplice> {
        let old_kinds = &self.allowed_hint_kinds;
        if new_kinds == old_kinds {
            return None;
        }

        let mut to_remove = Vec::new();
        let mut to_insert = Vec::new();
        let mut shown_hints_to_remove = visible_hints.iter().fold(
            HashMap::<ExcerptId, Vec<(Anchor, InlayId)>>::default(),
            |mut current_hints, inlay| {
                current_hints
                    .entry(inlay.position.excerpt_id)
                    .or_default()
                    .push((inlay.position, inlay.id));
                current_hints
            },
        );

        let multi_buffer = multi_buffer.read(cx);
        let multi_buffer_snapshot = multi_buffer.snapshot(cx);

        for (excerpt_id, excerpt_cached_hints) in &self.hints {
            let shown_excerpt_hints_to_remove =
                shown_hints_to_remove.entry(*excerpt_id).or_default();
            let excerpt_cached_hints = excerpt_cached_hints.read();
            let mut excerpt_cache = excerpt_cached_hints.hints.iter().fuse().peekable();
            shown_excerpt_hints_to_remove.retain(|(shown_anchor, shown_hint_id)| {
                let Some(buffer) = shown_anchor
                    .buffer_id
                    .and_then(|buffer_id| multi_buffer.buffer(buffer_id)) else { return false };
                let buffer_snapshot = buffer.read(cx).snapshot();
                loop {
                    match excerpt_cache.peek() {
                        Some((cached_hint_id, cached_hint)) => {
                            if cached_hint_id == shown_hint_id {
                                excerpt_cache.next();
                                return !new_kinds.contains(&cached_hint.kind);
                            }

                            match cached_hint
                                .position
                                .cmp(&shown_anchor.text_anchor, &buffer_snapshot)
                            {
                                cmp::Ordering::Less | cmp::Ordering::Equal => {
                                    if !old_kinds.contains(&cached_hint.kind)
                                        && new_kinds.contains(&cached_hint.kind)
                                    {
                                        to_insert.push((
                                            multi_buffer_snapshot.anchor_in_excerpt(
                                                *excerpt_id,
                                                cached_hint.position,
                                            ),
                                            *cached_hint_id,
                                            cached_hint.clone(),
                                        ));
                                    }
                                    excerpt_cache.next();
                                }
                                cmp::Ordering::Greater => return true,
                            }
                        }
                        None => return true,
                    }
                }
            });

            for (cached_hint_id, maybe_missed_cached_hint) in excerpt_cache {
                let cached_hint_kind = maybe_missed_cached_hint.kind;
                if !old_kinds.contains(&cached_hint_kind) && new_kinds.contains(&cached_hint_kind) {
                    to_insert.push((
                        multi_buffer_snapshot
                            .anchor_in_excerpt(*excerpt_id, maybe_missed_cached_hint.position),
                        *cached_hint_id,
                        maybe_missed_cached_hint.clone(),
                    ));
                }
            }
        }

        to_remove.extend(
            shown_hints_to_remove
                .into_values()
                .flatten()
                .map(|(_, hint_id)| hint_id),
        );
        if to_remove.is_empty() && to_insert.is_empty() {
            None
        } else {
            Some(InlaySplice {
                to_remove,
                to_insert,
            })
        }
    }

    fn clear(&mut self) {
        self.version += 1;
        self.update_tasks.clear();
        self.hints.clear();
    }
}

fn spawn_new_update_tasks(
    editor: &mut Editor,
    excerpts_to_query: HashMap<ExcerptId, (ModelHandle<Buffer>, Range<usize>)>,
    invalidation_strategy: InvalidationStrategy,
    update_cache_version: usize,
    cx: &mut ViewContext<'_, '_, Editor>,
) {
    let visible_hints = Arc::new(editor.visible_inlay_hints(cx));
    for (excerpt_id, (buffer_handle, excerpt_visible_range)) in excerpts_to_query {
        if !excerpt_visible_range.is_empty() {
            let buffer = buffer_handle.read(cx);
            let buffer_snapshot = buffer.snapshot();
            let cached_excerpt_hints = editor.inlay_hint_cache.hints.get(&excerpt_id).cloned();
            let cache_is_empty = match &cached_excerpt_hints {
                Some(cached_excerpt_hints) => {
                    let new_task_buffer_version = buffer_snapshot.version();
                    let cached_excerpt_hints = cached_excerpt_hints.read();
                    let cached_buffer_version = &cached_excerpt_hints.buffer_version;
                    if cached_excerpt_hints.version > update_cache_version
                        || cached_buffer_version.changed_since(new_task_buffer_version)
                    {
                        return;
                    }
                    if !new_task_buffer_version.changed_since(&cached_buffer_version)
                        && !matches!(invalidation_strategy, InvalidationStrategy::Forced)
                    {
                        return;
                    }

                    cached_excerpt_hints.hints.is_empty()
                }
                None => true,
            };

            let buffer_id = buffer.remote_id();
            let excerpt_visible_range_start = buffer.anchor_before(excerpt_visible_range.start);
            let excerpt_visible_range_end = buffer.anchor_after(excerpt_visible_range.end);

            let (multi_buffer_snapshot, full_excerpt_range) =
                editor.buffer.update(cx, |multi_buffer, cx| {
                    let multi_buffer_snapshot = multi_buffer.snapshot(cx);
                    (
                        multi_buffer_snapshot,
                        multi_buffer
                            .excerpts_for_buffer(&buffer_handle, cx)
                            .into_iter()
                            .find(|(id, _)| id == &excerpt_id)
                            .map(|(_, range)| range.context),
                    )
                });

            if let Some(full_excerpt_range) = full_excerpt_range {
                let query = ExcerptQuery {
                    buffer_id,
                    excerpt_id,
                    dimensions: ExcerptDimensions {
                        excerpt_range_start: full_excerpt_range.start,
                        excerpt_range_end: full_excerpt_range.end,
                        excerpt_visible_range_start,
                        excerpt_visible_range_end,
                    },
                    cache_version: update_cache_version,
                    invalidate: invalidation_strategy,
                };

                let new_update_task = |previous_task| {
                    new_update_task(
                        query,
                        multi_buffer_snapshot,
                        buffer_snapshot,
                        Arc::clone(&visible_hints),
                        cached_excerpt_hints,
                        previous_task,
                        cx,
                    )
                };
                match editor.inlay_hint_cache.update_tasks.entry(excerpt_id) {
                    hash_map::Entry::Occupied(mut o) => {
                        let update_task = o.get_mut();
                        if update_task.is_running() {
                            match (update_task.invalidation_strategy(), invalidation_strategy) {
                                (InvalidationStrategy::Forced, _)
                                | (_, InvalidationStrategy::OnConflict) => {
                                    o.insert(UpdateTask::new(
                                        invalidation_strategy,
                                        new_update_task(None),
                                    ));
                                }
                                (_, InvalidationStrategy::Forced) => {
                                    if cache_is_empty {
                                        o.insert(UpdateTask::new(
                                            invalidation_strategy,
                                            new_update_task(None),
                                        ));
                                    } else if update_task.pending_refresh.is_none() {
                                        update_task.pending_refresh = Some(new_update_task(Some(
                                            update_task.current.1.is_running_rx.clone(),
                                        )));
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            o.insert(UpdateTask::new(
                                invalidation_strategy,
                                new_update_task(None),
                            ));
                        }
                    }
                    hash_map::Entry::Vacant(v) => {
                        v.insert(UpdateTask::new(
                            invalidation_strategy,
                            new_update_task(None),
                        ));
                    }
                }
            }
        }
    }
}

fn new_update_task(
    query: ExcerptQuery,
    multi_buffer_snapshot: MultiBufferSnapshot,
    buffer_snapshot: BufferSnapshot,
    visible_hints: Arc<Vec<Inlay>>,
    cached_excerpt_hints: Option<Arc<RwLock<CachedExcerptHints>>>,
    task_before_refresh: Option<smol::channel::Receiver<()>>,
    cx: &mut ViewContext<'_, '_, Editor>,
) -> SpawnedTask {
    let hints_fetch_tasks = query.hints_fetch_ranges(&buffer_snapshot);
    let (is_running_tx, is_running_rx) = smol::channel::bounded(1);
    let is_refresh_task = task_before_refresh.is_some();
    let _task = cx.spawn(|editor, cx| async move {
        let _is_running_tx = is_running_tx;
        if let Some(task_before_refresh) = task_before_refresh {
            task_before_refresh.recv().await.ok();
        }
        let create_update_task = |range| {
            fetch_and_update_hints(
                editor.clone(),
                multi_buffer_snapshot.clone(),
                buffer_snapshot.clone(),
                Arc::clone(&visible_hints),
                cached_excerpt_hints.as_ref().map(Arc::clone),
                query,
                range,
                cx.clone(),
            )
        };

        if is_refresh_task {
            let visible_range_has_updates =
                match create_update_task(hints_fetch_tasks.visible_range).await {
                    Ok(updated) => updated,
                    Err(e) => {
                        error!("inlay hint visible range update task failed: {e:#}");
                        return;
                    }
                };

            if visible_range_has_updates {
                let other_update_results = futures::future::join_all(
                    hints_fetch_tasks
                        .other_ranges
                        .into_iter()
                        .map(create_update_task),
                )
                .await;

                for result in other_update_results {
                    if let Err(e) = result {
                        error!("inlay hint update task failed: {e:#}");
                        return;
                    }
                }
            }
        } else {
            let task_update_results = futures::future::join_all(
                std::iter::once(hints_fetch_tasks.visible_range)
                    .chain(hints_fetch_tasks.other_ranges.into_iter())
                    .map(create_update_task),
            )
            .await;

            for result in task_update_results {
                if let Err(e) = result {
                    error!("inlay hint update task failed: {e:#}");
                }
            }
        }
    });

    SpawnedTask {
        version: query.cache_version,
        _task,
        is_running_rx,
    }
}

async fn fetch_and_update_hints(
    editor: gpui::WeakViewHandle<Editor>,
    multi_buffer_snapshot: MultiBufferSnapshot,
    buffer_snapshot: BufferSnapshot,
    visible_hints: Arc<Vec<Inlay>>,
    cached_excerpt_hints: Option<Arc<RwLock<CachedExcerptHints>>>,
    query: ExcerptQuery,
    fetch_range: Range<language::Anchor>,
    mut cx: gpui::AsyncAppContext,
) -> anyhow::Result<bool> {
    let inlay_hints_fetch_task = editor
        .update(&mut cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .buffer(query.buffer_id)
                .and_then(|buffer| {
                    let project = editor.project.as_ref()?;
                    Some(project.update(cx, |project, cx| {
                        project.inlay_hints(buffer, fetch_range.clone(), cx)
                    }))
                })
        })
        .ok()
        .flatten();
    let mut update_happened = false;
    let Some(inlay_hints_fetch_task) = inlay_hints_fetch_task else { return Ok(update_happened) };

    let new_hints = inlay_hints_fetch_task
        .await
        .context("inlay hint fetch task")?;
    let background_task_buffer_snapshot = buffer_snapshot.clone();
    let backround_fetch_range = fetch_range.clone();
    if let Some(new_update) = cx
        .background()
        .spawn(async move {
            calculate_hint_updates(
                query,
                backround_fetch_range,
                new_hints,
                &background_task_buffer_snapshot,
                cached_excerpt_hints,
                &visible_hints,
            )
        })
        .await
    {
        update_happened = !new_update.add_to_cache.is_empty()
            || !new_update.remove_from_cache.is_empty()
            || !new_update.remove_from_visible.is_empty();
        editor
            .update(&mut cx, |editor, cx| {
                let cached_excerpt_hints = editor
                    .inlay_hint_cache
                    .hints
                    .entry(new_update.excerpt_id)
                    .or_insert_with(|| {
                        Arc::new(RwLock::new(CachedExcerptHints {
                            version: new_update.cache_version,
                            buffer_version: buffer_snapshot.version().clone(),
                            hints: Vec::new(),
                        }))
                    });
                let mut cached_excerpt_hints = cached_excerpt_hints.write();
                match new_update.cache_version.cmp(&cached_excerpt_hints.version) {
                    cmp::Ordering::Less => return,
                    cmp::Ordering::Greater | cmp::Ordering::Equal => {
                        cached_excerpt_hints.version = new_update.cache_version;
                    }
                }
                cached_excerpt_hints
                    .hints
                    .retain(|(hint_id, _)| !new_update.remove_from_cache.contains(hint_id));
                cached_excerpt_hints.buffer_version = buffer_snapshot.version().clone();
                editor.inlay_hint_cache.version += 1;

                let mut splice = InlaySplice {
                    to_remove: new_update.remove_from_visible,
                    to_insert: Vec::new(),
                };

                for new_hint in new_update.add_to_cache {
                    let new_hint_position = multi_buffer_snapshot
                        .anchor_in_excerpt(query.excerpt_id, new_hint.position);
                    let new_inlay_id = InlayId::Hint(post_inc(&mut editor.next_inlay_id));
                    if editor
                        .inlay_hint_cache
                        .allowed_hint_kinds
                        .contains(&new_hint.kind)
                    {
                        splice
                            .to_insert
                            .push((new_hint_position, new_inlay_id, new_hint.clone()));
                    }

                    cached_excerpt_hints.hints.push((new_inlay_id, new_hint));
                }

                cached_excerpt_hints
                    .hints
                    .sort_by(|(_, hint_a), (_, hint_b)| {
                        hint_a.position.cmp(&hint_b.position, &buffer_snapshot)
                    });
                drop(cached_excerpt_hints);

                let InlaySplice {
                    to_remove,
                    to_insert,
                } = splice;
                if !to_remove.is_empty() || !to_insert.is_empty() {
                    editor.splice_inlay_hints(to_remove, to_insert, cx)
                }
            })
            .ok();
    }

    Ok(update_happened)
}

fn calculate_hint_updates(
    query: ExcerptQuery,
    fetch_range: Range<language::Anchor>,
    new_excerpt_hints: Vec<InlayHint>,
    buffer_snapshot: &BufferSnapshot,
    cached_excerpt_hints: Option<Arc<RwLock<CachedExcerptHints>>>,
    visible_hints: &[Inlay],
) -> Option<ExcerptHintsUpdate> {
    let mut add_to_cache: HashSet<InlayHint> = HashSet::default();
    let mut excerpt_hints_to_persist = HashMap::default();
    for new_hint in new_excerpt_hints {
        if !contains_position(&fetch_range, new_hint.position, buffer_snapshot) {
            continue;
        }
        let missing_from_cache = match &cached_excerpt_hints {
            Some(cached_excerpt_hints) => {
                let cached_excerpt_hints = cached_excerpt_hints.read();
                match cached_excerpt_hints.hints.binary_search_by(|probe| {
                    probe.1.position.cmp(&new_hint.position, buffer_snapshot)
                }) {
                    Ok(ix) => {
                        let (cached_inlay_id, cached_hint) = &cached_excerpt_hints.hints[ix];
                        if cached_hint == &new_hint {
                            excerpt_hints_to_persist.insert(*cached_inlay_id, cached_hint.kind);
                            false
                        } else {
                            true
                        }
                    }
                    Err(_) => true,
                }
            }
            None => true,
        };
        if missing_from_cache {
            add_to_cache.insert(new_hint);
        }
    }

    let mut remove_from_visible = Vec::new();
    let mut remove_from_cache = HashSet::default();
    if matches!(
        query.invalidate,
        InvalidationStrategy::Forced | InvalidationStrategy::OnConflict
    ) {
        remove_from_visible.extend(
            visible_hints
                .iter()
                .filter(|hint| hint.position.excerpt_id == query.excerpt_id)
                .filter(|hint| {
                    contains_position(&fetch_range, hint.position.text_anchor, buffer_snapshot)
                })
                .filter(|hint| {
                    fetch_range
                        .start
                        .cmp(&hint.position.text_anchor, buffer_snapshot)
                        .is_le()
                        && fetch_range
                            .end
                            .cmp(&hint.position.text_anchor, buffer_snapshot)
                            .is_ge()
                })
                .map(|inlay_hint| inlay_hint.id)
                .filter(|hint_id| !excerpt_hints_to_persist.contains_key(hint_id)),
        );

        if let Some(cached_excerpt_hints) = &cached_excerpt_hints {
            let cached_excerpt_hints = cached_excerpt_hints.read();
            remove_from_cache.extend(
                cached_excerpt_hints
                    .hints
                    .iter()
                    .filter(|(cached_inlay_id, _)| {
                        !excerpt_hints_to_persist.contains_key(cached_inlay_id)
                    })
                    .filter(|(_, cached_hint)| {
                        fetch_range
                            .start
                            .cmp(&cached_hint.position, buffer_snapshot)
                            .is_le()
                            && fetch_range
                                .end
                                .cmp(&cached_hint.position, buffer_snapshot)
                                .is_ge()
                    })
                    .map(|(cached_inlay_id, _)| *cached_inlay_id),
            );
        }
    }

    if remove_from_visible.is_empty() && remove_from_cache.is_empty() && add_to_cache.is_empty() {
        None
    } else {
        Some(ExcerptHintsUpdate {
            cache_version: query.cache_version,
            excerpt_id: query.excerpt_id,
            remove_from_visible,
            remove_from_cache,
            add_to_cache,
        })
    }
}

struct HintFetchRanges {
    visible_range: Range<language::Anchor>,
    other_ranges: Vec<Range<language::Anchor>>,
}

fn contains_position(
    range: &Range<language::Anchor>,
    position: language::Anchor,
    buffer_snapshot: &BufferSnapshot,
) -> bool {
    range.start.cmp(&position, buffer_snapshot).is_le()
        && range.end.cmp(&position, buffer_snapshot).is_ge()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::{serde_json::json, InlayHintSettings};
    use futures::StreamExt;
    use gpui::{TestAppContext, ViewHandle};
    use language::{
        language_settings::AllLanguageSettingsContent, FakeLspAdapter, Language, LanguageConfig,
    };
    use lsp::FakeLanguageServer;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::Workspace;

    use crate::editor_tests::update_test_settings;

    use super::*;

    #[gpui::test]
    async fn test_basic_cache_update_with_duplicate_hints(cx: &mut gpui::TestAppContext) {
        let allowed_hint_kinds = HashSet::from_iter([None, Some(InlayHintKind::Type)]);
        init_test(cx, |settings| {
            settings.defaults.inlay_hints = Some(InlayHintSettings {
                enabled: true,
                show_type_hints: allowed_hint_kinds.contains(&Some(InlayHintKind::Type)),
                show_parameter_hints: allowed_hint_kinds.contains(&Some(InlayHintKind::Parameter)),
                show_other_hints: allowed_hint_kinds.contains(&None),
            })
        });
        let (file_with_hints, editor, fake_server) = prepare_test_objects(cx).await;
        let lsp_request_count = Arc::new(AtomicU32::new(0));
        fake_server
            .handle_request::<lsp::request::InlayHintRequest, _, _>(move |params, _| {
                let task_lsp_request_count = Arc::clone(&lsp_request_count);
                async move {
                    assert_eq!(
                        params.text_document.uri,
                        lsp::Url::from_file_path(file_with_hints).unwrap(),
                    );
                    let current_call_id =
                        Arc::clone(&task_lsp_request_count).fetch_add(1, Ordering::SeqCst);
                    let mut new_hints = Vec::with_capacity(2 * current_call_id as usize);
                    for _ in 0..2 {
                        let mut i = current_call_id;
                        loop {
                            new_hints.push(lsp::InlayHint {
                                position: lsp::Position::new(0, i),
                                label: lsp::InlayHintLabel::String(i.to_string()),
                                kind: None,
                                text_edits: None,
                                tooltip: None,
                                padding_left: None,
                                padding_right: None,
                                data: None,
                            });
                            if i == 0 {
                                break;
                            }
                            i -= 1;
                        }
                    }

                    Ok(Some(new_hints))
                }
            })
            .next()
            .await;
        cx.foreground().finish_waiting();
        cx.foreground().run_until_parked();
        let mut edits_made = 1;
        editor.update(cx, |editor, cx| {
            let expected_layers = vec!["0".to_string()];
            assert_eq!(
                expected_layers,
                cached_hint_labels(editor),
                "Should get its first hints when opening the editor"
            );
            assert_eq!(expected_layers, visible_hint_labels(editor, cx));
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, allowed_hint_kinds,
                "Cache should use editor settings to get the allowed hint kinds"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor update the cache version after every cache/view change"
            );
        });

        editor.update(cx, |editor, cx| {
            editor.change_selections(None, cx, |s| s.select_ranges([13..13]));
            editor.handle_input("some change", cx);
            edits_made += 1;
        });
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            let expected_layers = vec!["0".to_string(), "1".to_string()];
            assert_eq!(
                expected_layers,
                cached_hint_labels(editor),
                "Should get new hints after an edit"
            );
            assert_eq!(expected_layers, visible_hint_labels(editor, cx));
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, allowed_hint_kinds,
                "Cache should use editor settings to get the allowed hint kinds"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor update the cache version after every cache/view change"
            );
        });

        fake_server
            .request::<lsp::request::InlayHintRefreshRequest>(())
            .await
            .expect("inlay refresh request failed");
        edits_made += 1;
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            let expected_layers = vec!["0".to_string(), "1".to_string(), "2".to_string()];
            assert_eq!(
                expected_layers,
                cached_hint_labels(editor),
                "Should get new hints after hint refresh/ request"
            );
            assert_eq!(expected_layers, visible_hint_labels(editor, cx));
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, allowed_hint_kinds,
                "Cache should use editor settings to get the allowed hint kinds"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor update the cache version after every cache/view change"
            );
        });
    }

    async fn prepare_test_objects(
        cx: &mut TestAppContext,
    ) -> (&'static str, ViewHandle<Editor>, FakeLanguageServer) {
        let mut language = Language::new(
            LanguageConfig {
                name: "Rust".into(),
                path_suffixes: vec!["rs".to_string()],
                ..Default::default()
            },
            Some(tree_sitter_rust::language()),
        );
        let mut fake_servers = language
            .set_fake_lsp_adapter(Arc::new(FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    inlay_hint_provider: Some(lsp::OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            }))
            .await;

        let fs = FakeFs::new(cx.background());
        fs.insert_tree(
            "/a",
            json!({
                "main.rs": "fn main() { a } // and some long comment to ensure inlays are not trimmed out",
                "other.rs": "// Test file",
            }),
        )
        .await;

        let project = Project::test(fs, ["/a".as_ref()], cx).await;
        project.update(cx, |project, _| project.languages().add(Arc::new(language)));
        let (_, workspace) = cx.add_window(|cx| Workspace::test_new(project, cx));
        let worktree_id = workspace.update(cx, |workspace, cx| {
            workspace.project().read_with(cx, |project, cx| {
                project.worktrees(cx).next().unwrap().read(cx).id()
            })
        });

        cx.foreground().start_waiting();
        let editor = workspace
            .update(cx, |workspace, cx| {
                workspace.open_path((worktree_id, "main.rs"), None, true, cx)
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();

        let fake_server = fake_servers.next().await.unwrap();

        ("/a/main.rs", editor, fake_server)
    }

    #[gpui::test]
    async fn test_hint_setting_changes(cx: &mut gpui::TestAppContext) {
        let allowed_hint_kinds = HashSet::from_iter([None, Some(InlayHintKind::Type)]);
        init_test(cx, |settings| {
            settings.defaults.inlay_hints = Some(InlayHintSettings {
                enabled: true,
                show_type_hints: allowed_hint_kinds.contains(&Some(InlayHintKind::Type)),
                show_parameter_hints: allowed_hint_kinds.contains(&Some(InlayHintKind::Parameter)),
                show_other_hints: allowed_hint_kinds.contains(&None),
            })
        });
        let (file_with_hints, editor, fake_server) = prepare_test_objects(cx).await;
        let lsp_request_count = Arc::new(AtomicU32::new(0));
        let another_lsp_request_count = Arc::clone(&lsp_request_count);
        fake_server
            .handle_request::<lsp::request::InlayHintRequest, _, _>(move |params, _| {
                let task_lsp_request_count = Arc::clone(&another_lsp_request_count);
                async move {
                    Arc::clone(&task_lsp_request_count).fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        params.text_document.uri,
                        lsp::Url::from_file_path(file_with_hints).unwrap(),
                    );
                    Ok(Some(vec![
                        lsp::InlayHint {
                            position: lsp::Position::new(0, 1),
                            label: lsp::InlayHintLabel::String("type hint".to_string()),
                            kind: Some(lsp::InlayHintKind::TYPE),
                            text_edits: None,
                            tooltip: None,
                            padding_left: None,
                            padding_right: None,
                            data: None,
                        },
                        lsp::InlayHint {
                            position: lsp::Position::new(0, 2),
                            label: lsp::InlayHintLabel::String("parameter hint".to_string()),
                            kind: Some(lsp::InlayHintKind::PARAMETER),
                            text_edits: None,
                            tooltip: None,
                            padding_left: None,
                            padding_right: None,
                            data: None,
                        },
                        lsp::InlayHint {
                            position: lsp::Position::new(0, 3),
                            label: lsp::InlayHintLabel::String("other hint".to_string()),
                            kind: None,
                            text_edits: None,
                            tooltip: None,
                            padding_left: None,
                            padding_right: None,
                            data: None,
                        },
                    ]))
                }
            })
            .next()
            .await;
        cx.foreground().finish_waiting();
        cx.foreground().run_until_parked();

        let mut edits_made = 1;
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                1,
                "Should query new hints once"
            );
            assert_eq!(
                vec![
                    "type hint".to_string(),
                    "parameter hint".to_string(),
                    "other hint".to_string()
                ],
                cached_hint_labels(editor),
                "Should get its first hints when opening the editor"
            );
            assert_eq!(
                vec!["type hint".to_string(), "other hint".to_string()],
                visible_hint_labels(editor, cx)
            );
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, allowed_hint_kinds,
                "Cache should use editor settings to get the allowed hint kinds"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor update the cache version after every cache/view change"
            );
        });

        fake_server
            .request::<lsp::request::InlayHintRefreshRequest>(())
            .await
            .expect("inlay refresh request failed");
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                2,
                "Should load new hints twice"
            );
            assert_eq!(
                vec![
                    "type hint".to_string(),
                    "parameter hint".to_string(),
                    "other hint".to_string()
                ],
                cached_hint_labels(editor),
                "Cached hints should not change due to allowed hint kinds settings update"
            );
            assert_eq!(
                vec!["type hint".to_string(), "other hint".to_string()],
                visible_hint_labels(editor, cx)
            );
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(inlay_cache.allowed_hint_kinds, allowed_hint_kinds);
            assert_eq!(
                inlay_cache.version, edits_made,
                "Should not update cache version due to new loaded hints being the same"
            );
        });

        for (new_allowed_hint_kinds, expected_visible_hints) in [
            (HashSet::from_iter([None]), vec!["other hint".to_string()]),
            (
                HashSet::from_iter([Some(InlayHintKind::Type)]),
                vec!["type hint".to_string()],
            ),
            (
                HashSet::from_iter([Some(InlayHintKind::Parameter)]),
                vec!["parameter hint".to_string()],
            ),
            (
                HashSet::from_iter([None, Some(InlayHintKind::Type)]),
                vec!["type hint".to_string(), "other hint".to_string()],
            ),
            (
                HashSet::from_iter([None, Some(InlayHintKind::Parameter)]),
                vec!["parameter hint".to_string(), "other hint".to_string()],
            ),
            (
                HashSet::from_iter([Some(InlayHintKind::Type), Some(InlayHintKind::Parameter)]),
                vec!["type hint".to_string(), "parameter hint".to_string()],
            ),
            (
                HashSet::from_iter([
                    None,
                    Some(InlayHintKind::Type),
                    Some(InlayHintKind::Parameter),
                ]),
                vec![
                    "type hint".to_string(),
                    "parameter hint".to_string(),
                    "other hint".to_string(),
                ],
            ),
        ] {
            edits_made += 1;
            update_test_settings(cx, |settings| {
                settings.defaults.inlay_hints = Some(InlayHintSettings {
                    enabled: true,
                    show_type_hints: new_allowed_hint_kinds.contains(&Some(InlayHintKind::Type)),
                    show_parameter_hints: new_allowed_hint_kinds
                        .contains(&Some(InlayHintKind::Parameter)),
                    show_other_hints: new_allowed_hint_kinds.contains(&None),
                })
            });
            cx.foreground().run_until_parked();
            editor.update(cx, |editor, cx| {
                assert_eq!(
                    lsp_request_count.load(Ordering::Relaxed),
                    2,
                    "Should not load new hints on allowed hint kinds change for hint kinds {new_allowed_hint_kinds:?}"
                );
                assert_eq!(
                    vec![
                        "type hint".to_string(),
                        "parameter hint".to_string(),
                        "other hint".to_string(),
                    ],
                    cached_hint_labels(editor),
                    "Should get its cached hints unchanged after the settings change for hint kinds {new_allowed_hint_kinds:?}"
                );
                assert_eq!(
                    expected_visible_hints,
                    visible_hint_labels(editor, cx),
                    "Should get its visible hints filtered after the settings change for hint kinds {new_allowed_hint_kinds:?}"
                );
                let inlay_cache = editor.inlay_hint_cache();
                assert_eq!(
                    inlay_cache.allowed_hint_kinds, new_allowed_hint_kinds,
                    "Cache should use editor settings to get the allowed hint kinds for hint kinds {new_allowed_hint_kinds:?}"
                );
                assert_eq!(
                    inlay_cache.version, edits_made,
                    "The editor should update the cache version after every cache/view change for hint kinds {new_allowed_hint_kinds:?} due to visible hints change"
                );
            });
        }

        edits_made += 1;
        let another_allowed_hint_kinds = HashSet::from_iter([Some(InlayHintKind::Type)]);
        update_test_settings(cx, |settings| {
            settings.defaults.inlay_hints = Some(InlayHintSettings {
                enabled: false,
                show_type_hints: another_allowed_hint_kinds.contains(&Some(InlayHintKind::Type)),
                show_parameter_hints: another_allowed_hint_kinds
                    .contains(&Some(InlayHintKind::Parameter)),
                show_other_hints: another_allowed_hint_kinds.contains(&None),
            })
        });
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                2,
                "Should not load new hints when hints got disabled"
            );
            assert!(
                cached_hint_labels(editor).is_empty(),
                "Should clear the cache when hints got disabled"
            );
            assert!(
                visible_hint_labels(editor, cx).is_empty(),
                "Should clear visible hints when hints got disabled"
            );
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, another_allowed_hint_kinds,
                "Should update its allowed hint kinds even when hints got disabled"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor should update the cache version after hints got disabled"
            );
        });

        fake_server
            .request::<lsp::request::InlayHintRefreshRequest>(())
            .await
            .expect("inlay refresh request failed");
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                2,
                "Should not load new hints when they got disabled"
            );
            assert!(cached_hint_labels(editor).is_empty());
            assert!(visible_hint_labels(editor, cx).is_empty());
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(inlay_cache.allowed_hint_kinds, another_allowed_hint_kinds);
            assert_eq!(
                inlay_cache.version, edits_made,
                "The editor should not update the cache version after /refresh query without updates"
            );
        });

        let final_allowed_hint_kinds = HashSet::from_iter([Some(InlayHintKind::Parameter)]);
        edits_made += 1;
        update_test_settings(cx, |settings| {
            settings.defaults.inlay_hints = Some(InlayHintSettings {
                enabled: true,
                show_type_hints: final_allowed_hint_kinds.contains(&Some(InlayHintKind::Type)),
                show_parameter_hints: final_allowed_hint_kinds
                    .contains(&Some(InlayHintKind::Parameter)),
                show_other_hints: final_allowed_hint_kinds.contains(&None),
            })
        });
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                3,
                "Should query for new hints when they got reenabled"
            );
            assert_eq!(
                vec![
                    "type hint".to_string(),
                    "parameter hint".to_string(),
                    "other hint".to_string(),
                ],
                cached_hint_labels(editor),
                "Should get its cached hints fully repopulated after the hints got reenabled"
            );
            assert_eq!(
                vec!["parameter hint".to_string()],
                visible_hint_labels(editor, cx),
                "Should get its visible hints repopulated and filtered after the h"
            );
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(
                inlay_cache.allowed_hint_kinds, final_allowed_hint_kinds,
                "Cache should update editor settings when hints got reenabled"
            );
            assert_eq!(
                inlay_cache.version, edits_made,
                "Cache should update its version after hints got reenabled"
            );
        });

        fake_server
            .request::<lsp::request::InlayHintRefreshRequest>(())
            .await
            .expect("inlay refresh request failed");
        cx.foreground().run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(
                lsp_request_count.load(Ordering::Relaxed),
                4,
                "Should query for new hints again"
            );
            assert_eq!(
                vec![
                    "type hint".to_string(),
                    "parameter hint".to_string(),
                    "other hint".to_string(),
                ],
                cached_hint_labels(editor),
            );
            assert_eq!(
                vec!["parameter hint".to_string()],
                visible_hint_labels(editor, cx),
            );
            let inlay_cache = editor.inlay_hint_cache();
            assert_eq!(inlay_cache.allowed_hint_kinds, final_allowed_hint_kinds,);
            assert_eq!(inlay_cache.version, edits_made);
        });
    }

    pub(crate) fn init_test(cx: &mut TestAppContext, f: impl Fn(&mut AllLanguageSettingsContent)) {
        cx.foreground().forbid_parking();

        cx.update(|cx| {
            cx.set_global(SettingsStore::test(cx));
            theme::init((), cx);
            client::init_settings(cx);
            language::init(cx);
            Project::init_settings(cx);
            workspace::init_settings(cx);
            crate::init(cx);
        });

        update_test_settings(cx, f);
    }

    fn cached_hint_labels(editor: &Editor) -> Vec<String> {
        let mut labels = Vec::new();
        for (_, excerpt_hints) in &editor.inlay_hint_cache().hints {
            let excerpt_hints = excerpt_hints.read();
            for (_, inlay) in excerpt_hints.hints.iter() {
                match &inlay.label {
                    project::InlayHintLabel::String(s) => labels.push(s.to_string()),
                    _ => unreachable!(),
                }
            }
        }
        labels
    }

    fn visible_hint_labels(editor: &Editor, cx: &ViewContext<'_, '_, Editor>) -> Vec<String> {
        editor
            .visible_inlay_hints(cx)
            .into_iter()
            .map(|hint| hint.text.to_string())
            .collect()
    }
}
