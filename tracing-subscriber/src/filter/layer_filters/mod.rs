//! ## Per-Layer Filtering
//!
//! Per-layer filters permit individual `Layer`s to have their own filter
//! configurations without interfering with other `Layer`s.
//!
//! This module is not public; the public APIs defined in this module are
//! re-exported in the top-level `filter` module. Therefore, this documentation
//! primarily concerns the internal implementation details. For the user-facing
//! public API documentation, see the individual public types in this module, as
//! well as the, see the `Layer` trait documentation's [per-layer filtering
//! section]][1].
//!
//! ## How does per-layer filtering work?
//!
//! As described in the API documentation, the [`Filter`] trait defines a
//! filtering strategy for a per-layer filter. We expect there will be a variety
//! of implementations of [`Filter`], both in `tracing-subscriber` and in user
//! code.
//!
//! To actually *use* a [`Filter`] implementation, it is combined with a
//! [`Layer`] by the [`Filtered`] struct defined in this module. [`Filtered`]
//! implements [`Layer`] by calling into the wrapped [`Layer`], or not, based on
//! the filtering strategy. While there will be a variety of types that implement
//! [`Filter`], all actual *uses* of per-layer filtering will occur through the
//! [`Filtered`] struct. Therefore, most of the implementation details live
//! there.
//!
//! [1]: crate::layer#per-layer-filtering
//! [`Filter`]: crate::layer::Filter
use crate::{
    filter::LevelFilter,
    layer::{self, Context, Layer},
    registry,
};
use std::{
    any::TypeId,
    cell::{Cell, RefCell},
    fmt,
    marker::PhantomData,
    ops::Deref,
    sync::Arc,
    thread_local,
};
use tracing_core::{
    span,
    subscriber::{Interest, Subscriber},
    Event, Metadata,
};
pub mod combinator;

/// A [`Layer`] that wraps an inner [`Layer`] and adds a [`Filter`] which
/// controls what spans and events are enabled for that layer.
///
/// This is returned by the [`Layer::with_filter`] method. See the
/// [documentation on per-layer filtering][plf] for details.
///
/// [`Filter`]: crate::layer::Filter
/// [plf]: crate::layer#per-layer-filtering
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
#[derive(Clone)]
pub struct Filtered<L, F, S> {
    filter: F,
    layer: L,
    id: MagicPlfDowncastMarker,
    _s: PhantomData<fn(S)>,
}

/// Uniquely identifies an individual [`Filter`] instance in the context of
/// a [`Subscriber`].
///
/// When adding a [`Filtered`] [`Layer`] to a [`Subscriber`], the [`Subscriber`]
/// generates a `FilterId` for that [`Filtered`] layer. The [`Filtered`] layer
/// will then use the generated ID to query whether a particular span was
/// previously enabled by that layer's [`Filter`].
///
/// **Note**: Currently, the [`Registry`] type provided by this crate is the
/// **only** [`Subscriber`] implementation capable of participating in per-layer
/// filtering. Therefore, the `FilterId` type cannot currently be constructed by
/// code outside of `tracing-subscriber`. In the future, new APIs will be added to `tracing-subscriber` to
/// allow non-Registry [`Subscriber`]s to also participate in per-layer
/// filtering. When those APIs are added, subscribers will be responsible
/// for generating and assigning `FilterId`s.
///
/// [`Filter`]: crate::layer::Filter
/// [`Subscriber`]: tracing_core::Subscriber
/// [`Layer`]: crate::layer::Layer
/// [`Registry`]: crate::registry::Registry
#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
#[derive(Copy, Clone)]
pub struct FilterId(u64);

/// A bitmap tracking which [`FilterId`]s have enabled a given span or
/// event.
///
/// This is currently a private type that's used exclusively by the
/// [`Registry`]. However, in the future, this may become a public API, in order
/// to allow user subscribers to host [`Filter`]s.
///
/// [`Registry`]: crate::Registry
/// [`Filter`]: crate::layer::Filter
#[derive(Default, Copy, Clone, Eq, PartialEq)]
pub(crate) struct FilterMap {
    bits: u64,
}

/// The current state of `enabled` calls to per-layer filters on this
/// thread.
///
/// When `Filtered::enabled` is called, the filter will set the bit
/// corresponding to its ID if the filter will disable the event/span being
/// filtered. When the event or span is recorded, the per-layer filter will
/// check its bit to determine if it disabled that event or span, and skip
/// forwarding the event or span to the inner layer if the bit is set. Once
/// a span or event has been skipped by a per-layer filter, it unsets its
/// bit, so that the `FilterMap` has been cleared for the next set of
/// `enabled` calls.
///
/// FilterState is also read by the `Registry`, for two reasons:
///
/// 1. When filtering a span, the Registry must store the `FilterMap`
///    generated by `Filtered::enabled` calls for that span as part of the
///    span's per-span data. This allows `Filtered` layers to determine
///    whether they had previously disabled a given span, and avoid showing it
///    to the wrapped layer if it was disabled.
///
///    This allows `Filtered` layers to also filter out the spans they
///    disable from span traversals (such as iterating over parents, etc).
/// 2. If all the bits are set, then every per-layer filter has decided it
///    doesn't want to enable that span or event. In that case, the
///    `Registry`'s `enabled` method will return `false`, so that
///     recording a span or event can be skipped entirely.
#[derive(Debug)]
pub(crate) struct FilterState {
    enabled: Cell<FilterMap>,
    // TODO(eliza): `Interest`s should _probably_ be `Copy`. The only reason
    // they're not is our Obsessive Commitment to Forwards-Compatibility. If
    // this changes in tracing-core`, we can make this a `Cell` rather than
    // `RefCell`...
    interest: RefCell<Option<Interest>>,

    #[cfg(debug_assertions)]
    counters: DebugCounters,
}

/// Extra counters added to `FilterState` used only to make debug assertions.
#[cfg(debug_assertions)]
#[derive(Debug, Default)]
struct DebugCounters {
    /// How many per-layer filters have participated in the current `enabled`
    /// call?
    in_filter_pass: Cell<usize>,

    /// How many per-layer filters have participated in the current `register_callsite`
    /// call?
    in_interest_pass: Cell<usize>,
}

thread_local! {
    pub(crate) static FILTERING: FilterState = FilterState::new();
}

/// Extension trait adding [combinators] for combining [`Filter`].
///
/// [combinators]: crate::filter::combinator
/// [`Filter`]: crate::layer::Filter
pub trait FilterExt<S>: layer::Filter<S> {
    /// Combines this [`Filter`] with another [`Filter`] s so that spans and
    /// events are enabled if and only if *both* filters return `true`.
    ///
    /// # Examples
    ///
    /// Enabling spans or events if they have both a particular target *and* are
    /// above a certain level:
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     filter::{filter_fn, LevelFilter, FilterExt},
    ///     prelude::*,
    /// };
    ///
    /// // Enables spans and events with targets starting with `interesting_target`:
    /// let target_filter = filter_fn(|meta| {
    ///     meta.target().starts_with("interesting_target")
    /// });
    ///
    /// // Enables spans and events with levels `INFO` and below:
    /// let level_filter = LevelFilter::INFO;
    ///
    /// // Combine the two filters together, returning a filter that only enables
    /// // spans and events that *both* filters will enable:
    /// let filter = target_filter.and(level_filter);
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer().with_filter(filter))
    ///     .init();
    ///
    /// // This event will *not* be enabled:
    /// tracing::info!("an event with an uninteresting target");
    ///
    /// // This event *will* be enabled:
    /// tracing::info!(target: "interesting_target", "a very interesting event");
    ///
    /// // This event will *not* be enabled:
    /// tracing::debug!(target: "interesting_target", "interesting debug event...");
    /// ```
    ///
    /// [`Filter`]: crate::layer::Filter
    fn and<B>(self, other: B) -> combinator::And<Self, B, S>
    where
        Self: Sized,
        B: layer::Filter<S>,
    {
        combinator::And::new(self, other)
    }

    /// Combines two [`Filter`]s so that spans and events are enabled if *either* filter
    /// returns `true`.
    ///
    /// # Examples
    ///
    /// Enabling spans and events at the `INFO` level and above, and all spans
    /// and events with a particular target:
    /// ```
    /// use tracing_subscriber::{
    ///     filter::{filter_fn, LevelFilter, FilterExt},
    ///     prelude::*,
    /// };
    ///
    /// // Enables spans and events with targets starting with `interesting_target`:
    /// let target_filter = filter_fn(|meta| {
    ///     meta.target().starts_with("interesting_target")
    /// });
    ///
    /// // Enables spans and events with levels `INFO` and below:
    /// let level_filter = LevelFilter::INFO;
    ///
    /// // Combine the two filters together so that a span or event is enabled
    /// // if it is at INFO or lower, or if it has a target starting with
    /// // `interesting_target`.
    /// let filter = level_filter.or(target_filter);
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer().with_filter(filter))
    ///     .init();
    ///
    /// // This event will *not* be enabled:
    /// tracing::debug!("an uninteresting event");
    ///
    /// // This event *will* be enabled:
    /// tracing::info!("an uninteresting INFO event");
    ///
    /// // This event *will* be enabled:
    /// tracing::info!(target: "interesting_target", "a very interesting event");
    ///
    /// // This event *will* be enabled:
    /// tracing::debug!(target: "interesting_target", "interesting debug event...");
    /// ```
    ///
    /// Enabling a higher level for a particular target by using `or` in
    /// conjunction with the [`and`] combinator:
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     filter::{filter_fn, LevelFilter, FilterExt},
    ///     prelude::*,
    /// };
    ///
    /// // This filter will enable spans and events with targets beginning with
    /// // `my_crate`:
    /// let my_crate = filter_fn(|meta| {
    ///     meta.target().starts_with("my_crate")
    /// });
    ///
    /// let filter = my_crate
    ///     // Combine the `my_crate` filter with a `LevelFilter` to produce a
    ///     // filter that will enable the `INFO` level and lower for spans and
    ///     // events with `my_crate` targets:
    ///     .and(LevelFilter::INFO)
    ///     // If a span or event *doesn't* have a target beginning with
    ///     // `my_crate`, enable it if it has the `WARN` level or lower:
    ///     .or(LevelFilter::WARN);
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer().with_filter(filter))
    ///     .init();
    /// ```
    ///
    /// [`Filter`]: crate::layer::Filter
    /// [`and`]: FilterExt::and
    fn or<B>(self, other: B) -> combinator::Or<Self, B, S>
    where
        Self: Sized,
        B: layer::Filter<S>,
    {
        combinator::Or::new(self, other)
    }

    /// Inverts `self`, returning a filter that enables spans and events only if
    /// `self` would *not* enable them.
    fn not(self) -> combinator::Not<Self, S>
    where
        Self: Sized,
    {
        combinator::Not::new(self)
    }

    /// [Boxes] `self`, erasing its concrete type.
    ///
    /// This is equivalent to calling [`Box::new`], but in method form, so that
    /// it can be used when chaining combinator methods.
    ///
    /// # Examples
    ///
    /// When different combinations of filters are used conditionally, they may
    /// have different types. For example, the following code won't compile,
    /// since the `if` and `else` clause produce filters of different types:
    ///
    /// ```compile_fail
    /// use tracing_subscriber::{
    ///     filter::{filter_fn, LevelFilter, FilterExt},
    ///     prelude::*,
    /// };
    ///
    /// let enable_bar_target: bool = // ...
    /// # false;
    ///
    /// let filter = if enable_bar_target {
    ///     filter_fn(|meta| meta.target().starts_with("foo"))
    ///         // If `enable_bar_target` is true, add a `filter_fn` enabling
    ///         // spans and events with the target `bar`:
    ///         .or(filter_fn(|meta| meta.target().starts_with("bar")))
    ///         .and(LevelFilter::INFO)
    /// } else {
    ///     filter_fn(|meta| meta.target().starts_with("foo"))
    ///         .and(LevelFilter::INFO)
    /// };
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer().with_filter(filter))
    ///     .init();
    /// ```
    ///
    /// By using `boxed`, the types of the two different branches can be erased,
    /// so the assignment to the `filter` variable is valid (as both branches
    /// have the type `Box<dyn Filter<S> + Send + Sync + 'static>`). The
    /// following code *does* compile:
    ///
    /// ```
    /// use tracing_subscriber::{
    ///     filter::{filter_fn, LevelFilter, FilterExt},
    ///     prelude::*,
    /// };
    ///
    /// let enable_bar_target: bool = // ...
    /// # false;
    ///
    /// let filter = if enable_bar_target {
    ///     filter_fn(|meta| meta.target().starts_with("foo"))
    ///         .or(filter_fn(|meta| meta.target().starts_with("bar")))
    ///         .and(LevelFilter::INFO)
    ///         // Boxing the filter erases its type, so both branches now
    ///         // have the same type.
    ///         .boxed()
    /// } else {
    ///     filter_fn(|meta| meta.target().starts_with("foo"))
    ///         .and(LevelFilter::INFO)
    ///         .boxed()
    /// };
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer().with_filter(filter))
    ///     .init();
    /// ```
    ///
    /// [Boxes]: std::boxed
    /// [`Box::new`]: std::boxed::Box::new
    fn boxed(self) -> Box<dyn layer::Filter<S> + Send + Sync + 'static>
    where
        Self: Sized + Send + Sync + 'static,
    {
        Box::new(self)
    }
}

// === impl Filter ===

#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
impl<S> layer::Filter<S> for LevelFilter {
    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        meta.level() <= self
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if meta.level() <= self {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(*self)
    }
}

macro_rules! filter_impl_body {
    () => {
        #[inline]
        fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
            self.deref().enabled(meta, cx)
        }

        #[inline]
        fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
            self.deref().callsite_enabled(meta)
        }

        #[inline]
        fn max_level_hint(&self) -> Option<LevelFilter> {
            self.deref().max_level_hint()
        }
    };
}

#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
impl<S> layer::Filter<S> for Arc<dyn layer::Filter<S> + Send + Sync + 'static> {
    filter_impl_body!();
}

#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(feature = "registry")))]
impl<S> layer::Filter<S> for Box<dyn layer::Filter<S> + Send + Sync + 'static> {
    filter_impl_body!();
}

// === impl Filtered ===

impl<L, F, S> Filtered<L, F, S> {
    /// Wraps the provided [`Layer`] so that it is filtered by the given
    /// [`Filter`].
    ///
    /// This is equivalent to calling the [`Layer::with_filter`] method.
    ///
    /// See the [documentation on per-layer filtering][plf] for details.
    ///
    /// [`Filter`]: crate::layer::Filter
    /// [plf]: crate::layer#per-layer-filtering
    pub fn new(layer: L, filter: F) -> Self {
        Self {
            layer,
            filter,
            id: MagicPlfDowncastMarker(FilterId::disabled()),
            _s: PhantomData,
        }
    }

    #[inline(always)]
    fn id(&self) -> FilterId {
        debug_assert!(
            !self.id.0.is_disabled(),
            "a `Filtered` layer was used, but it had no `FilterId`; \
            was it registered with the subscriber?"
        );
        self.id.0
    }

    fn did_enable(&self, f: impl FnOnce()) {
        FILTERING.with(|filtering| filtering.did_enable(self.id(), f))
    }

    /// Gives mutable access to the inner filter, which allows swapping the filter inside a reload handle's [`modify`] method.
    /// See https://github.com/tokio-rs/tracing/issues/1629#issuecomment-1013538461
    pub fn filter_mut(&mut self) -> &mut F {
        &mut self.filter
    }
}

impl<S, L, F> Layer<S> for Filtered<L, F, S>
where
    S: Subscriber + for<'span> registry::LookupSpan<'span> + 'static,
    F: layer::Filter<S> + 'static,
    L: Layer<S>,
{
    fn on_layer(&mut self, subscriber: &mut S) {
        self.id = MagicPlfDowncastMarker(subscriber.register_filter());
        self.layer.on_layer(subscriber);
    }

    // TODO(eliza): can we figure out a nice way to make the `Filtered` layer
    // not call `is_enabled_for` in hooks that the inner layer doesn't actually
    // have real implementations of? probably not...
    //
    // it would be cool if there was some wild rust reflection way of checking
    // if a trait impl has the default impl of a trait method or not, but that's
    // almsot certainly impossible...right?

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        let interest = self.filter.callsite_enabled(metadata);

        // If the filter didn't disable the callsite, allow the inner layer to
        // register it — since `register_callsite` is also used for purposes
        // such as reserving/caching per-callsite data, we want the inner layer
        // to be able to perform any other registration steps. However, we'll
        // ignore its `Interest`.
        if !interest.is_never() {
            self.layer.register_callsite(metadata);
        }

        // Add our `Interest` to the current sum of per-layer filter `Interest`s
        // for this callsite.
        FILTERING.with(|filtering| filtering.add_interest(interest));

        // don't short circuit! if the stack consists entirely of `Layer`s with
        // per-layer filters, the `Registry` will return the actual `Interest`
        // value that's the sum of all the `register_callsite` calls to those
        // per-layer filters. if we returned an actual `never` interest here, a
        // `Layered` layer would short-circuit and not allow any `Filtered`
        // layers below us if _they_ are interested in the callsite.
        Interest::always()
    }

    fn enabled(&self, metadata: &Metadata<'_>, cx: Context<'_, S>) -> bool {
        let cx = cx.with_filter(self.id());
        let enabled = self.filter.enabled(metadata, &cx);
        FILTERING.with(|filtering| filtering.set(self.id(), enabled));

        if enabled {
            // If the filter enabled this metadata, ask the wrapped layer if
            // _it_ wants it --- it might have a global filter.
            self.layer.enabled(metadata, cx)
        } else {
            // Otherwise, return `true`. The _per-layer_ filter disabled this
            // metadata, but returning `false` in `Layer::enabled` will
            // short-circuit and globally disable the span or event. This is
            // *not* what we want for per-layer filters, as other layers may
            // still want this event. Returning `true` here means we'll continue
            // asking the next layer in the stack.
            //
            // Once all per-layer filters have been evaluated, the `Registry`
            // at the root of the stack will return `false` from its `enabled`
            // method if *every* per-layer  filter disabled this metadata.
            // Otherwise, the individual per-layer filters will skip the next
            // `new_span` or `on_event` call for their layer if *they* disabled
            // the span or event, but it was not globally disabled.
            true
        }
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.on_new_span(attrs, id, cx.with_filter(self.id()));
        })
    }

    #[doc(hidden)]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.filter.max_level_hint()
    }

    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(span, self.id()) {
            self.layer.on_record(span, values, cx)
        }
    }

    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, cx: Context<'_, S>) {
        // only call `on_follows_from` if both spans are enabled by us
        if cx.is_enabled_for(span, self.id()) && cx.is_enabled_for(follows, self.id()) {
            self.layer
                .on_follows_from(span, follows, cx.with_filter(self.id()))
        }
    }

    fn on_event(&self, event: &Event<'_>, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.on_event(event, cx.with_filter(self.id()));
        })
    }

    fn on_enter(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id()) {
            self.layer.on_enter(id, cx)
        }
    }

    fn on_exit(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id()) {
            self.layer.on_exit(id, cx)
        }
    }

    fn on_close(&self, id: span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(&id, self.id()) {
            self.layer.on_close(id, cx)
        }
    }

    // XXX(eliza): the existence of this method still makes me sad...
    fn on_id_change(&self, old: &span::Id, new: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(old, self.id()) {
            self.layer.on_id_change(old, new, cx)
        }
    }

    #[doc(hidden)]
    #[inline]
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        match id {
            id if id == TypeId::of::<Self>() => Some(self as *const _ as *const ()),
            id if id == TypeId::of::<L>() => Some(&self.layer as *const _ as *const ()),
            id if id == TypeId::of::<F>() => Some(&self.filter as *const _ as *const ()),
            id if id == TypeId::of::<MagicPlfDowncastMarker>() => {
                Some(&self.id as *const _ as *const ())
            }
            _ => self.layer.downcast_raw(id),
        }
    }
}

impl<F, L, S> fmt::Debug for Filtered<F, L, S>
where
    F: fmt::Debug,
    L: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Filtered")
            .field("filter", &self.filter)
            .field("layer", &self.layer)
            .field("id", &self.id)
            .finish()
    }
}

// === impl FilterId ===

impl FilterId {
    const fn disabled() -> Self {
        Self(std::u64::MAX)
    }

    /// Returns a `FilterId` that will consider _all_ spans enabled.
    pub(crate) const fn none() -> Self {
        Self(0)
    }

    pub(crate) fn new(id: u8) -> Self {
        assert!(id < 64, "filter IDs may not be greater than 64");
        Self(1 << id as usize)
    }

    /// Combines two `FilterId`s, returning a new `FilterId` that will match a
    /// [`FilterMap`] where the span was disabled by _either_ this `FilterId`
    /// *or* the combined `FilterId`.
    ///
    /// This method is called by [`Context`]s when adding the `FilterId` of a
    /// [`Filtered`] layer to the context.
    ///
    /// This is necessary for cases where we have a tree of nested [`Filtered`]
    /// layers, like this:
    ///
    /// ```text
    /// Filtered {
    ///     filter1,
    ///     Layered {
    ///         layer1,
    ///         Filtered {
    ///              filter2,
    ///              layer2,
    ///         },
    /// }
    /// ```
    ///
    /// We want `layer2` to be affected by both `filter1` _and_ `filter2`.
    /// Without combining `FilterId`s, this works fine when filtering
    /// `on_event`/`new_span`, because the outer `Filtered` layer (`filter1`)
    /// won't call the inner layer's `on_event` or `new_span` callbacks if it
    /// disabled the event/span.
    ///
    /// However, it _doesn't_ work when filtering span lookups and traversals
    /// (e.g. `scope`). This is because the [`Context`] passed to `layer2`
    /// would set its filter ID to the filter ID of `filter2`, and would skip
    /// spans that were disabled by `filter2`. However, what if a span was
    /// disabled by `filter1`? We wouldn't see it in `new_span`, but we _would_
    /// see it in lookups and traversals...which we don't want.
    ///
    /// When a [`Filtered`] layer adds its ID to a [`Context`], it _combines_ it
    /// with any previous filter ID that the context had, rather than replacing
    /// it. That way, `layer2`'s context will check if a span was disabled by
    /// `filter1` _or_ `filter2`. The way we do this, instead of representing
    /// `FilterId`s as a number number that we shift a 1 over by to get a mask,
    /// we just store the actual mask,so we can combine them with a bitwise-OR.
    ///
    /// For example, if we consider the following case (pretending that the
    /// masks are 8 bits instead of 64 just so i don't have to write out a bunch
    /// of extra zeroes):
    ///
    /// - `filter1` has the filter id 1 (`0b0000_0001`)
    /// - `filter2` has the filter id 2 (`0b0000_0010`)
    ///
    /// A span that gets disabled by filter 1 would have the [`FilterMap`] with
    /// bits `0b0000_0001`.
    ///
    /// If the `FilterId` was internally represented as `(bits to shift + 1),
    /// when `layer2`'s [`Context`] checked if it enabled the  span, it would
    /// make the mask `0b0000_0010` (`1 << 1`). That bit would not be set in the
    /// [`FilterMap`], so it would see that it _didn't_ disable  the span. Which
    /// is *true*, it just doesn't reflect the tree-like shape of the actual
    /// subscriber.
    ///
    /// By having the IDs be masks instead of shifts, though, when the
    /// [`Filtered`] with `filter2` gets the [`Context`] with `filter1`'s filter ID,
    /// instead of replacing it, it ors them together:
    ///
    /// ```ignore
    /// 0b0000_0001 | 0b0000_0010 == 0b0000_0011;
    /// ```
    ///
    /// We then test if the span was disabled by  seeing if _any_ bits in the
    /// mask are `1`:
    ///
    /// ```ignore
    /// filtermap & mask != 0;
    /// 0b0000_0001 & 0b0000_0011 != 0;
    /// 0b0000_0001 != 0;
    /// true;
    /// ```
    ///
    /// [`Context`]: crate::layer::Context
    pub(crate) fn and(self, FilterId(other): Self) -> Self {
        // If this mask is disabled, just return the other --- otherwise, we
        // would always see that every span is disabled.
        if self.0 == Self::disabled().0 {
            return Self(other);
        }

        Self(self.0 | other)
    }

    fn is_disabled(self) -> bool {
        self.0 == Self::disabled().0
    }
}

impl fmt::Debug for FilterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // don't print a giant set of the numbers 0..63 if the filter ID is disabled.
        if self.0 == Self::disabled().0 {
            return f
                .debug_tuple("FilterId")
                .field(&format_args!("DISABLED"))
                .finish();
        }

        if f.alternate() {
            f.debug_struct("FilterId")
                .field("ids", &format_args!("{:?}", FmtBitset(self.0)))
                .field("bits", &format_args!("{:b}", self.0))
                .finish()
        } else {
            f.debug_tuple("FilterId").field(&FmtBitset(self.0)).finish()
        }
    }
}

impl fmt::Binary for FilterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FilterId")
            .field(&format_args!("{:b}", self.0))
            .finish()
    }
}

// === impl FilterExt ===

impl<F, S> FilterExt<S> for F where F: layer::Filter<S> {}

// === impl FilterMap ===

impl FilterMap {
    pub(crate) fn set(self, FilterId(mask): FilterId, enabled: bool) -> Self {
        if mask == std::u64::MAX {
            return self;
        }

        if enabled {
            Self {
                bits: self.bits & (!mask),
            }
        } else {
            Self {
                bits: self.bits | mask,
            }
        }
    }

    #[inline]
    pub(crate) fn is_enabled(self, FilterId(mask): FilterId) -> bool {
        self.bits & mask == 0
    }

    #[inline]
    pub(crate) fn any_enabled(self) -> bool {
        self.bits != std::u64::MAX
    }
}

impl fmt::Debug for FilterMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alt = f.alternate();
        let mut s = f.debug_struct("FilterMap");
        s.field("disabled_by", &format_args!("{:?}", &FmtBitset(self.bits)));

        if alt {
            s.field("bits", &format_args!("{:b}", self.bits));
        }

        s.finish()
    }
}

impl fmt::Binary for FilterMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterMap")
            .field("bits", &format_args!("{:b}", self.bits))
            .finish()
    }
}

// === impl FilterState ===

impl FilterState {
    fn new() -> Self {
        Self {
            enabled: Cell::new(FilterMap::default()),
            interest: RefCell::new(None),

            #[cfg(debug_assertions)]
            counters: DebugCounters::default(),
        }
    }

    fn set(&self, filter: FilterId, enabled: bool) {
        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_filter_pass.get();
            if in_current_pass == 0 {
                debug_assert_eq!(self.enabled.get(), FilterMap::default());
            }
            self.counters.in_filter_pass.set(in_current_pass + 1);
            debug_assert_eq!(
                self.counters.in_interest_pass.get(),
                0,
                "if we are in or starting a filter pass, we must not be in an interest pass."
            )
        }

        self.enabled.set(self.enabled.get().set(filter, enabled))
    }

    fn add_interest(&self, interest: Interest) {
        let mut curr_interest = self.interest.borrow_mut();

        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_interest_pass.get();
            if in_current_pass == 0 {
                debug_assert!(curr_interest.is_none());
            }
            self.counters.in_interest_pass.set(in_current_pass + 1);
        }

        if let Some(curr_interest) = curr_interest.as_mut() {
            if (curr_interest.is_always() && !interest.is_always())
                || (curr_interest.is_never() && !interest.is_never())
            {
                *curr_interest = Interest::sometimes();
            }
            // If the two interests are the same, do nothing. If the current
            // interest is `sometimes`, stay sometimes.
        } else {
            *curr_interest = Some(interest);
        }
    }

    pub(crate) fn event_enabled() -> bool {
        FILTERING
            .try_with(|this| {
                let enabled = this.enabled.get().any_enabled();
                #[cfg(debug_assertions)]
                {
                    if this.counters.in_filter_pass.get() == 0 {
                        debug_assert_eq!(this.enabled.get(), FilterMap::default());
                    }

                    // Nothing enabled this event, we won't tick back down the
                    // counter in `did_enable`. Reset it.
                    if !enabled {
                        this.counters.in_filter_pass.set(0);
                    }
                }
                enabled
            })
            .unwrap_or(true)
    }

    /// Executes a closure if the filter with the provided ID did not disable
    /// the current span/event.
    ///
    /// This is used to implement the `on_event` and `new_span` methods for
    /// `Filtered`.
    fn did_enable(&self, filter: FilterId, f: impl FnOnce()) {
        let map = self.enabled.get();
        if map.is_enabled(filter) {
            // If the filter didn't disable the current span/event, run the
            // callback.
            f();
        } else {
            // Otherwise, if this filter _did_ disable the span or event
            // currently being processed, clear its bit from this thread's
            // `FilterState`. The bit has already been "consumed" by skipping
            // this callback, and we need to ensure that the `FilterMap` for
            // this thread is reset when the *next* `enabled` call occurs.
            self.enabled.set(map.set(filter, true));
        }
        #[cfg(debug_assertions)]
        {
            let in_current_pass = self.counters.in_filter_pass.get();
            if in_current_pass <= 1 {
                debug_assert_eq!(self.enabled.get(), FilterMap::default());
            }
            self.counters
                .in_filter_pass
                .set(in_current_pass.saturating_sub(1));
            debug_assert_eq!(
                self.counters.in_interest_pass.get(),
                0,
                "if we are in a filter pass, we must not be in an interest pass."
            )
        }
    }

    /// Clears the current in-progress filter state.
    ///
    /// This resets the [`FilterMap`] and current [`Interest`] as well as
    /// clearing the debug counters.
    pub(crate) fn clear_enabled() {
        // Drop the `Result` returned by `try_with` --- if we are in the middle
        // a panic and the thread-local has been torn down, that's fine, just
        // ignore it ratehr than panicking.
        let _ = FILTERING.try_with(|filtering| {
            filtering.enabled.set(FilterMap::default());

            #[cfg(debug_assertions)]
            filtering.counters.in_filter_pass.set(0);
        });
    }

    pub(crate) fn take_interest() -> Option<Interest> {
        FILTERING
            .try_with(|filtering| {
                #[cfg(debug_assertions)]
                {
                    if filtering.counters.in_interest_pass.get() == 0 {
                        debug_assert!(filtering.interest.try_borrow().ok()?.is_none());
                    }
                    filtering.counters.in_interest_pass.set(0);
                }
                filtering.interest.try_borrow_mut().ok()?.take()
            })
            .ok()?
    }

    pub(crate) fn filter_map(&self) -> FilterMap {
        let map = self.enabled.get();
        #[cfg(debug_assertions)]
        {
            if self.counters.in_filter_pass.get() == 0 {
                debug_assert_eq!(map, FilterMap::default());
            }
        }

        map
    }
}
/// This is a horrible and bad abuse of the downcasting system to expose
/// *internally* whether a layer has per-layer filtering, within
/// `tracing-subscriber`, without exposing a public API for it.
///
/// If a `Layer` has per-layer filtering, it will downcast to a
/// `MagicPlfDowncastMarker`. Since layers which contain other layers permit
/// downcasting to recurse to their children, this will do the Right Thing with
/// layers like Reload, Option, etc.
///
/// Why is this a wrapper around the `FilterId`, you may ask? Because
/// downcasting works by returning a pointer, and we don't want to risk
/// introducing UB by  constructing pointers that _don't_ point to a valid
/// instance of the type they claim to be. In this case, we don't _intend_ for
/// this pointer to be dereferenced, so it would actually be fine to return one
/// that isn't a valid pointer...but we can't guarantee that the caller won't
/// (accidentally) dereference it, so it's better to be safe than sorry. We
/// could, alternatively, add an additional field to the type that's used only
/// for returning pointers to as as part of the evil downcasting hack, but I
/// thought it was nicer to just add a `repr(transparent)` wrapper to the
/// existing `FilterId` field, since it won't make the struct any bigger.
///
/// Don't worry, this isn't on the test. :)
#[derive(Clone, Copy)]
#[repr(transparent)]
struct MagicPlfDowncastMarker(FilterId);
impl fmt::Debug for MagicPlfDowncastMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Just pretend that `MagicPlfDowncastMarker` doesn't exist for
        // `fmt::Debug` purposes...if no one *sees* it in their `Debug` output,
        // they don't have to know I thought this code would be a good idea.
        fmt::Debug::fmt(&self.0, f)
    }
}

pub(crate) fn is_plf_downcast_marker(type_id: TypeId) -> bool {
    type_id == TypeId::of::<MagicPlfDowncastMarker>()
}

/// Does a type implementing `Subscriber` contain any per-layer filters?
pub(crate) fn subscriber_has_plf<S>(subscriber: &S) -> bool
where
    S: Subscriber,
{
    (subscriber as &dyn Subscriber).is::<MagicPlfDowncastMarker>()
}

/// Does a type implementing `Layer` contain any per-layer filters?
pub(crate) fn layer_has_plf<L, S>(layer: &L) -> bool
where
    L: Layer<S>,
    S: Subscriber,
{
    unsafe {
        // Safety: we're not actually *doing* anything with this pointer --- we
        // only care about the `Option`, which we're turning into a `bool`. So
        // even if the layer decides to be evil and give us some kind of invalid
        // pointer, we don't ever dereference it, so this is always safe.
        layer.downcast_raw(TypeId::of::<MagicPlfDowncastMarker>())
    }
    .is_some()
}

struct FmtBitset(u64);

impl fmt::Debug for FmtBitset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = f.debug_set();
        for bit in 0..64 {
            // if the `bit`-th bit is set, add it to the debug set
            if self.0 & (1 << bit) != 0 {
                set.entry(&bit);
            }
        }
        set.finish()
    }
}
