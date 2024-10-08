//! Layout of content
//! - at the top-level, into a [`Document`].
//! - inside of a container, into a [`Frame`] or [`Fragment`].

use std::fmt::{self, Debug, Formatter};
use std::num::NonZeroUsize;
use std::ptr;

use comemo::{Track, Tracked, TrackedMut};

use crate::diag::{bail, SourceResult};
use crate::engine::{Engine, Route, Sink, Traced};
use crate::foundations::{
    elem, Args, Construct, Content, NativeElement, Packed, Resolve, Smart, StyleChain,
};
use crate::introspection::{
    Counter, CounterDisplayElem, CounterKey, Introspector, Locator, LocatorLink,
    ManualPageCounter, SplitLocator, Tag, TagElem,
};
use crate::layout::{
    Abs, AlignElem, Alignment, Axes, Binding, BlockElem, ColbreakElem, ColumnsElem, Dir,
    FixedAlignment, FlushElem, Fr, Fragment, Frame, FrameItem, HAlignment, Length,
    OuterVAlignment, Page, PageElem, Paper, Parity, PlaceElem, Point, Ratio, Region,
    Regions, Rel, Sides, Size, Spacing, VAlignment, VElem,
};
use crate::model::{Document, Numbering};
use crate::model::{FootnoteElem, FootnoteEntry, ParElem};
use crate::realize::StyleVec;
use crate::realize::{realize_flow, realize_root, Arenas};
use crate::text::TextElem;
use crate::utils::Numeric;
use crate::World;

/// Layout content into a document.
///
/// This first performs root-level realization and then lays out the resulting
/// elements. In contrast to [`layout_fragment`], this does not take regions
/// since the regions are defined by the page configuration in the content and
/// style chain.
#[typst_macros::time(name = "document")]
pub fn layout_document(
    engine: &mut Engine,
    content: &Content,
    styles: StyleChain,
) -> SourceResult<Document> {
    layout_document_impl(
        engine.world,
        engine.introspector,
        engine.traced,
        TrackedMut::reborrow_mut(&mut engine.sink),
        engine.route.track(),
        content,
        styles,
    )
}

/// The internal implementation of `layout_document`.
#[comemo::memoize]
fn layout_document_impl(
    world: Tracked<dyn World + '_>,
    introspector: Tracked<Introspector>,
    traced: Tracked<Traced>,
    sink: TrackedMut<Sink>,
    route: Tracked<Route>,
    content: &Content,
    styles: StyleChain,
) -> SourceResult<Document> {
    let mut locator = Locator::root().split();
    let mut engine = Engine {
        world,
        introspector,
        traced,
        sink,
        route: Route::extend(route).unnested(),
    };

    let arenas = Arenas::default();
    let (children, styles, info) =
        realize_root(&mut engine, &mut locator, &arenas, content, styles)?;

    let mut peekable = children.chain(&styles).peekable();
    let iter = std::iter::from_fn(|| {
        let (child, styles) = peekable.next()?;
        let extend_to = peekable
            .peek()
            .and_then(|(next, _)| *next.to_packed::<PageElem>()?.clear_to()?);
        let locator = locator.next(&child.span());
        Some((child, styles, extend_to, locator))
    });

    let layouts =
        engine.parallelize(iter, |engine, (child, styles, extend_to, locator)| {
            if let Some(page) = child.to_packed::<PageElem>() {
                layout_page_run(engine, page, locator, styles, extend_to)
            } else {
                bail!(child.span(), "expected page element");
            }
        });

    let mut page_counter = ManualPageCounter::new();
    let mut pages = Vec::with_capacity(children.len());
    for result in layouts {
        let layout = result?;
        pages.extend(finalize_page_run(&mut engine, layout, &mut page_counter)?);
    }

    Ok(Document { pages, info, introspector: Introspector::default() })
}

/// A prepared layout of a page run that can be finalized with access to the
/// page counter.
struct PageRunLayout<'a> {
    page: &'a Packed<PageElem>,
    locator: SplitLocator<'a>,
    styles: StyleChain<'a>,
    extend_to: Option<Parity>,
    area: Size,
    margin: Sides<Abs>,
    two_sided: bool,
    frames: Vec<Frame>,
}

/// A document can consist of multiple `PageElem`s, one per run of pages
/// with equal properties (not one per actual output page!). The `number` is
/// the physical page number of the first page of this run. It is mutated
/// while we post-process the pages in this function. This function returns
/// a fragment consisting of multiple frames, one per output page of this
/// page run.
#[typst_macros::time(name = "pages", span = page.span())]
fn layout_page_run<'a>(
    engine: &mut Engine,
    page: &'a Packed<PageElem>,
    locator: Locator<'a>,
    styles: StyleChain<'a>,
    extend_to: Option<Parity>,
) -> SourceResult<PageRunLayout<'a>> {
    let mut locator = locator.split();

    // When one of the lengths is infinite the page fits its content along
    // that axis.
    let width = page.width(styles).unwrap_or(Abs::inf());
    let height = page.height(styles).unwrap_or(Abs::inf());
    let mut size = Size::new(width, height);
    if page.flipped(styles) {
        std::mem::swap(&mut size.x, &mut size.y);
    }

    let mut min = width.min(height);
    if !min.is_finite() {
        min = Paper::A4.width();
    }

    // Determine the margins.
    let default = Rel::<Length>::from((2.5 / 21.0) * min);
    let margin = page.margin(styles);
    let two_sided = margin.two_sided.unwrap_or(false);
    let margin = margin
        .sides
        .map(|side| side.and_then(Smart::custom).unwrap_or(default))
        .resolve(styles)
        .relative_to(size);

    // Realize columns.
    let area = size - margin.sum_by_axis();
    let mut regions = Regions::repeat(area, area.map(Abs::is_finite));
    regions.root = true;

    // Layout the child.
    let columns = page.columns(styles);
    let fragment = if columns.get() > 1 {
        layout_fragment_with_columns(
            engine,
            &page.body,
            locator.next(&page.span()),
            styles,
            regions,
            columns,
            ColumnsElem::gutter_in(styles),
        )?
    } else {
        layout_fragment(engine, &page.body, locator.next(&page.span()), styles, regions)?
    };

    Ok(PageRunLayout {
        page,
        locator,
        styles,
        extend_to,
        area,
        margin,
        two_sided,
        frames: fragment.into_frames(),
    })
}

/// Finalize the layout with access to the next page counter.
#[typst_macros::time(name = "finalize pages", span = page.span())]
fn finalize_page_run(
    engine: &mut Engine,
    PageRunLayout {
        page,
        mut locator,
        styles,
        extend_to,
        area,
        margin,
        two_sided,
        mut frames,
    }: PageRunLayout<'_>,
    page_counter: &mut ManualPageCounter,
) -> SourceResult<Vec<Page>> {
    // Align the child to the pagebreak's parity.
    // Check for page count after adding the pending frames
    if extend_to.is_some_and(|p| !p.matches(page_counter.physical().get() + frames.len()))
    {
        // Insert empty page after the current pages.
        let size = area.map(Abs::is_finite).select(area, Size::zero());
        frames.push(Frame::hard(size));
    }

    let fill = page.fill(styles);
    let foreground = page.foreground(styles);
    let background = page.background(styles);
    let header_ascent = page.header_ascent(styles);
    let footer_descent = page.footer_descent(styles);
    let numbering = page.numbering(styles);
    let number_align = page.number_align(styles);
    let binding =
        page.binding(styles)
            .unwrap_or_else(|| match TextElem::dir_in(styles) {
                Dir::LTR => Binding::Left,
                _ => Binding::Right,
            });

    // Construct the numbering (for header or footer).
    let numbering_marginal = numbering.as_ref().map(|numbering| {
        let both = match numbering {
            Numbering::Pattern(pattern) => pattern.pieces() >= 2,
            Numbering::Func(_) => true,
        };

        let mut counter = CounterDisplayElem::new(
            Counter::new(CounterKey::Page),
            Smart::Custom(numbering.clone()),
            both,
        )
        .pack()
        .spanned(page.span());

        // We interpret the Y alignment as selecting header or footer
        // and then ignore it for aligning the actual number.
        if let Some(x) = number_align.x() {
            counter = counter.aligned(x.into());
        }

        counter
    });

    let header = page.header(styles);
    let footer = page.footer(styles);
    let (header, footer) = if matches!(number_align.y(), Some(OuterVAlignment::Top)) {
        (header.as_ref().unwrap_or(&numbering_marginal), footer.as_ref().unwrap_or(&None))
    } else {
        (header.as_ref().unwrap_or(&None), footer.as_ref().unwrap_or(&numbering_marginal))
    };

    // Post-process pages.
    let mut pages = Vec::with_capacity(frames.len());
    for mut frame in frames {
        // The padded width of the page's content without margins.
        let pw = frame.width();

        // If two sided, left becomes inside and right becomes outside.
        // Thus, for left-bound pages, we want to swap on even pages and
        // for right-bound pages, we want to swap on odd pages.
        let mut margin = margin;
        if two_sided && binding.swap(page_counter.physical()) {
            std::mem::swap(&mut margin.left, &mut margin.right);
        }

        // Realize margins.
        frame.set_size(frame.size() + margin.sum_by_axis());
        frame.translate(Point::new(margin.left, margin.top));

        // The page size with margins.
        let size = frame.size();

        // Realize overlays.
        for marginal in [header, footer, background, foreground] {
            let Some(content) = marginal.as_ref() else { continue };

            let (pos, area, align);
            if ptr::eq(marginal, header) {
                let ascent = header_ascent.relative_to(margin.top);
                pos = Point::with_x(margin.left);
                area = Size::new(pw, margin.top - ascent);
                align = Alignment::BOTTOM;
            } else if ptr::eq(marginal, footer) {
                let descent = footer_descent.relative_to(margin.bottom);
                pos = Point::new(margin.left, size.y - margin.bottom + descent);
                area = Size::new(pw, margin.bottom - descent);
                align = Alignment::TOP;
            } else {
                pos = Point::zero();
                area = size;
                align = HAlignment::Center + VAlignment::Horizon;
            };

            let aligned = content.clone().styled(AlignElem::set_alignment(align));
            let sub = layout_frame(
                engine,
                &aligned,
                locator.next(&content.span()),
                styles,
                Region::new(area, Axes::splat(true)),
            )?;

            if ptr::eq(marginal, header) || ptr::eq(marginal, background) {
                frame.prepend_frame(pos, sub);
            } else {
                frame.push_frame(pos, sub);
            }
        }

        page_counter.visit(engine, &frame)?;
        pages.push(Page {
            frame,
            fill: fill.clone(),
            numbering: numbering.clone(),
            number: page_counter.logical(),
        });

        page_counter.step();
    }

    Ok(pages)
}

/// Layout content into a single region.
pub fn layout_frame(
    engine: &mut Engine,
    content: &Content,
    locator: Locator,
    styles: StyleChain,
    region: Region,
) -> SourceResult<Frame> {
    layout_fragment(engine, content, locator, styles, region.into())
        .map(Fragment::into_frame)
}

/// Layout content into multiple regions.
///
/// When just layouting into a single region, prefer [`layout_frame`].
pub fn layout_fragment(
    engine: &mut Engine,
    content: &Content,
    locator: Locator,
    styles: StyleChain,
    regions: Regions,
) -> SourceResult<Fragment> {
    layout_fragment_impl(
        engine.world,
        engine.introspector,
        engine.traced,
        TrackedMut::reborrow_mut(&mut engine.sink),
        engine.route.track(),
        content,
        locator.track(),
        styles,
        regions,
    )
}

/// Layout content into regions with columns.
///
/// For now, this just invokes normal layout on cycled smaller regions. However,
/// in the future, columns will be able to interact (e.g. through floating
/// figures), so this is already factored out because it'll be conceptually
/// different from just layouting into more smaller regions.
pub fn layout_fragment_with_columns(
    engine: &mut Engine,
    content: &Content,
    locator: Locator,
    styles: StyleChain,
    regions: Regions,
    count: NonZeroUsize,
    gutter: Rel<Abs>,
) -> SourceResult<Fragment> {
    // Separating the infinite space into infinite columns does not make
    // much sense.
    if !regions.size.x.is_finite() {
        return layout_fragment(engine, content, locator, styles, regions);
    }

    // Determine the width of the gutter and each column.
    let count = count.get();
    let gutter = gutter.relative_to(regions.base().x);
    let width = (regions.size.x - gutter * (count - 1) as f64) / count as f64;

    let backlog: Vec<_> = std::iter::once(&regions.size.y)
        .chain(regions.backlog)
        .flat_map(|&height| std::iter::repeat(height).take(count))
        .skip(1)
        .collect();

    // Create the pod regions.
    let pod = Regions {
        size: Size::new(width, regions.size.y),
        full: regions.full,
        backlog: &backlog,
        last: regions.last,
        expand: Axes::new(true, regions.expand.y),
        root: regions.root,
    };

    // Layout the children.
    let mut frames = layout_fragment(engine, content, locator, styles, pod)?.into_iter();
    let mut finished = vec![];

    let dir = TextElem::dir_in(styles);
    let total_regions = (frames.len() as f32 / count as f32).ceil() as usize;

    // Stitch together the column for each region.
    for region in regions.iter().take(total_regions) {
        // The height should be the parent height if we should expand.
        // Otherwise its the maximum column height for the frame. In that
        // case, the frame is first created with zero height and then
        // resized.
        let height = if regions.expand.y { region.y } else { Abs::zero() };
        let mut output = Frame::hard(Size::new(regions.size.x, height));
        let mut cursor = Abs::zero();

        for _ in 0..count {
            let Some(frame) = frames.next() else { break };
            if !regions.expand.y {
                output.size_mut().y.set_max(frame.height());
            }

            let width = frame.width();
            let x =
                if dir == Dir::LTR { cursor } else { regions.size.x - cursor - width };

            output.push_frame(Point::with_x(x), frame);
            cursor += width + gutter;
        }

        finished.push(output);
    }

    Ok(Fragment::frames(finished))
}

/// The internal implementation of [`layout_fragment`].
#[allow(clippy::too_many_arguments)]
#[comemo::memoize]
fn layout_fragment_impl(
    world: Tracked<dyn World + '_>,
    introspector: Tracked<Introspector>,
    traced: Tracked<Traced>,
    sink: TrackedMut<Sink>,
    route: Tracked<Route>,
    content: &Content,
    locator: Tracked<Locator>,
    styles: StyleChain,
    regions: Regions,
) -> SourceResult<Fragment> {
    let link = LocatorLink::new(locator);
    let mut locator = Locator::link(&link).split();
    let mut engine = Engine {
        world,
        introspector,
        traced,
        sink,
        route: Route::extend(route),
    };

    if !engine.route.within(Route::MAX_LAYOUT_DEPTH) {
        bail!(
            content.span(), "maximum layout depth exceeded";
            hint: "try to reduce the amount of nesting in your layout",
        );
    }

    // If we are in a `PageElem`, this might already be a realized flow.
    if let Some(flow) = content.to_packed::<FlowElem>() {
        return FlowLayouter::new(&mut engine, flow, locator, &styles, regions).layout();
    }

    // Layout the content by first turning it into a `FlowElem` and then
    // layouting that.
    let arenas = Arenas::default();
    let (flow, styles) =
        realize_flow(&mut engine, &mut locator, &arenas, content, styles)?;

    FlowLayouter::new(&mut engine, &flow, locator, &styles, regions).layout()
}

/// A collection of block-level layoutable elements. This is analogous to a
/// paragraph, which is a collection of inline-level layoutable elements.
///
/// This element is responsible for layouting both the top-level content flow
/// and the contents of any containers.
#[elem(Debug, Construct)]
pub struct FlowElem {
    /// The children that will be arranged into a flow.
    #[internal]
    #[variadic]
    pub children: StyleVec,
}

impl Construct for FlowElem {
    fn construct(_: &mut Engine, args: &mut Args) -> SourceResult<Content> {
        bail!(args.span, "cannot be constructed manually");
    }
}

impl Debug for FlowElem {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Flow ")?;
        self.children.fmt(f)
    }
}

/// Performs flow layout.
struct FlowLayouter<'a, 'e> {
    /// The engine.
    engine: &'a mut Engine<'e>,
    /// The children that will be arranged into a flow.
    flow: &'a Packed<FlowElem>,
    /// Whether this is the root flow.
    root: bool,
    /// Provides unique locations to the flow's children.
    locator: SplitLocator<'a>,
    /// The shared styles.
    styles: &'a StyleChain<'a>,
    /// The regions to layout children into.
    regions: Regions<'a>,
    /// Whether the flow should expand to fill the region.
    expand: Axes<bool>,
    /// The initial size of `regions.size` that was available before we started
    /// subtracting.
    initial: Size,
    /// Whether the last block was a paragraph.
    ///
    /// Used for indenting paragraphs after the first in a block.
    last_was_par: bool,
    /// Spacing and layouted blocks for the current region.
    items: Vec<FlowItem>,
    /// A queue of tags that will be attached to the next frame.
    pending_tags: Vec<&'a Tag>,
    /// A queue of floating elements.
    pending_floats: Vec<FlowItem>,
    /// Whether we have any footnotes in the current region.
    has_footnotes: bool,
    /// Footnote configuration.
    footnote_config: FootnoteConfig,
    /// Finished frames for previous regions.
    finished: Vec<Frame>,
}

/// Cached footnote configuration.
struct FootnoteConfig {
    separator: Content,
    clearance: Abs,
    gap: Abs,
}

/// A prepared item in a flow layout.
#[derive(Debug)]
enum FlowItem {
    /// Spacing between other items and whether it is weak.
    Absolute(Abs, bool),
    /// Fractional spacing between other items.
    Fractional(Fr),
    /// A frame for a layouted block.
    Frame {
        /// The frame itself.
        frame: Frame,
        /// How to align the frame.
        align: Axes<FixedAlignment>,
        /// Whether the frame sticks to the item after it (for orphan prevention).
        sticky: bool,
        /// Whether the frame is movable; that is, kept together with its
        /// footnotes.
        ///
        /// This is true for frames created by paragraphs and
        /// [`BlockElem::single_layouter`] elements.
        movable: bool,
    },
    /// An absolutely placed frame.
    Placed {
        /// The layouted content.
        frame: Frame,
        /// Where to place the content horizontally.
        x_align: FixedAlignment,
        /// Where to place the content vertically.
        y_align: Smart<Option<FixedAlignment>>,
        /// A translation to apply to the content.
        delta: Axes<Rel<Abs>>,
        /// Whether the content floats --- i.e. collides with in-flow content.
        float: bool,
        /// The amount of space that needs to be kept between the placed content
        /// and in-flow content. Only relevant if `float` is `true`.
        clearance: Abs,
    },
    /// A footnote frame (can also be the separator).
    Footnote(Frame),
}

impl FlowItem {
    /// Whether this item is out-of-flow.
    ///
    /// Out-of-flow items are guaranteed to have a [zero size][Size::zero()].
    fn is_out_of_flow(&self) -> bool {
        match self {
            Self::Placed { float: false, .. } => true,
            Self::Frame { frame, .. } => {
                frame.size().is_zero()
                    && frame.items().all(|(_, item)| {
                        matches!(item, FrameItem::Link(_, _) | FrameItem::Tag(_))
                    })
            }
            _ => false,
        }
    }
}

impl<'a, 'e> FlowLayouter<'a, 'e> {
    /// Create a new flow layouter.
    fn new(
        engine: &'a mut Engine<'e>,
        flow: &'a Packed<FlowElem>,
        locator: SplitLocator<'a>,
        styles: &'a StyleChain<'a>,
        mut regions: Regions<'a>,
    ) -> Self {
        // Check whether we have just a single multiple-layoutable element. In
        // that case, we do not set `expand.y` to `false`, but rather keep it at
        // its original value (since that element can take the full space).
        //
        // Consider the following code: `block(height: 5cm, pad(10pt,
        // align(bottom, ..)))`. Thanks to the code below, the expansion will be
        // passed all the way through the block & pad and reach the innermost
        // flow, so that things are properly bottom-aligned.
        let mut alone = false;
        if let [child] = flow.children.elements() {
            alone = child.is::<BlockElem>();
        }

        // Disable vertical expansion when there are multiple or not directly
        // layoutable children.
        let expand = regions.expand;
        if !alone {
            regions.expand.y = false;
        }

        // The children aren't root.
        let root = std::mem::replace(&mut regions.root, false);

        Self {
            engine,
            flow,
            root,
            locator,
            styles,
            regions,
            expand,
            initial: regions.size,
            last_was_par: false,
            items: vec![],
            pending_tags: vec![],
            pending_floats: vec![],
            has_footnotes: false,
            footnote_config: FootnoteConfig {
                separator: FootnoteEntry::separator_in(*styles),
                clearance: FootnoteEntry::clearance_in(*styles),
                gap: FootnoteEntry::gap_in(*styles),
            },
            finished: vec![],
        }
    }

    /// Layout the flow.
    fn layout(mut self) -> SourceResult<Fragment> {
        for (child, styles) in self.flow.children.chain(self.styles) {
            if let Some(elem) = child.to_packed::<TagElem>() {
                self.handle_tag(elem);
            } else if let Some(elem) = child.to_packed::<VElem>() {
                self.handle_v(elem, styles)?;
            } else if let Some(elem) = child.to_packed::<ColbreakElem>() {
                self.handle_colbreak(elem)?;
            } else if let Some(elem) = child.to_packed::<ParElem>() {
                self.handle_par(elem, styles)?;
            } else if let Some(elem) = child.to_packed::<BlockElem>() {
                self.handle_block(elem, styles)?;
            } else if let Some(elem) = child.to_packed::<PlaceElem>() {
                self.handle_place(elem, styles)?;
            } else if let Some(elem) = child.to_packed::<FlushElem>() {
                self.handle_flush(elem)?;
            } else {
                bail!(child.span(), "unexpected flow child");
            }
        }

        self.finish()
    }

    /// Place explicit metadata into the flow.
    fn handle_tag(&mut self, elem: &'a Packed<TagElem>) {
        self.pending_tags.push(&elem.tag);
    }

    /// Layout vertical spacing.
    fn handle_v(&mut self, v: &'a Packed<VElem>, styles: StyleChain) -> SourceResult<()> {
        self.handle_item(match v.amount {
            Spacing::Rel(rel) => FlowItem::Absolute(
                // Resolve the spacing relative to the current base height.
                rel.resolve(styles).relative_to(self.initial.y),
                v.weakness(styles) > 0,
            ),
            Spacing::Fr(fr) => FlowItem::Fractional(fr),
        })
    }

    /// Layout a column break.
    fn handle_colbreak(&mut self, _: &'a Packed<ColbreakElem>) -> SourceResult<()> {
        // If there is still an available region, skip to it.
        // TODO: Turn this into a region abstraction.
        if !self.regions.backlog.is_empty() || self.regions.last.is_some() {
            self.finish_region(true)?;
        }
        Ok(())
    }

    /// Layout a paragraph.
    #[typst_macros::time(name = "par", span = par.span())]
    fn handle_par(
        &mut self,
        par: &'a Packed<ParElem>,
        styles: StyleChain,
    ) -> SourceResult<()> {
        // Fetch properties.
        let align = AlignElem::alignment_in(styles).resolve(styles);
        let leading = ParElem::leading_in(styles);
        let costs = TextElem::costs_in(styles);

        // Layout the paragraph into lines. This only depends on the base size,
        // not on the Y position.
        let consecutive = self.last_was_par;
        let locator = self.locator.next(&par.span());
        let lines = crate::layout::layout_inline(
            self.engine,
            &par.children,
            locator,
            styles,
            consecutive,
            self.regions.base(),
            self.regions.expand.x,
        )?
        .into_frames();

        // If the first line doesn’t fit in this region, then defer any
        // previous sticky frame to the next region (if available)
        if let Some(first) = lines.first() {
            while !self.regions.size.y.fits(first.height()) && !self.regions.in_last() {
                let in_last = self.finish_region_with_migration()?;
                if in_last {
                    break;
                }
            }
        }

        // Determine whether to prevent widow and orphans.
        let len = lines.len();
        let prevent_orphans =
            costs.orphan() > Ratio::zero() && len >= 2 && !lines[1].is_empty();
        let prevent_widows =
            costs.widow() > Ratio::zero() && len >= 2 && !lines[len - 2].is_empty();
        let prevent_all = len == 3 && prevent_orphans && prevent_widows;

        // Store the heights of lines at the edges because we'll potentially
        // need these later when `lines` is already moved.
        let height_at = |i| lines.get(i).map(Frame::height).unwrap_or_default();
        let front_1 = height_at(0);
        let front_2 = height_at(1);
        let back_2 = height_at(len.saturating_sub(2));
        let back_1 = height_at(len.saturating_sub(1));

        // Layout the lines.
        for (i, mut frame) in lines.into_iter().enumerate() {
            if i > 0 {
                self.handle_item(FlowItem::Absolute(leading, true))?;
            }

            // To prevent widows and orphans, we require enough space for
            // - all lines if it's just three
            // - the first two lines if we're at the first line
            // - the last two lines if we're at the second to last line
            let needed = if prevent_all && i == 0 {
                front_1 + leading + front_2 + leading + back_1
            } else if prevent_orphans && i == 0 {
                front_1 + leading + front_2
            } else if prevent_widows && i >= 2 && i + 2 == len {
                back_2 + leading + back_1
            } else {
                frame.height()
            };

            // If the line(s) don't fit into this region, but they do fit into
            // the next, then advance.
            if !self.regions.in_last()
                && !self.regions.size.y.fits(needed)
                && self.regions.iter().nth(1).is_some_and(|region| region.y.fits(needed))
            {
                self.finish_region(false)?;
            }

            self.drain_tag(&mut frame);
            self.handle_item(FlowItem::Frame {
                frame,
                align,
                sticky: false,
                movable: true,
            })?;
        }

        self.last_was_par = true;
        Ok(())
    }

    /// Layout into multiple regions.
    fn handle_block(
        &mut self,
        block: &'a Packed<BlockElem>,
        styles: StyleChain<'a>,
    ) -> SourceResult<()> {
        // Fetch properties.
        let sticky = block.sticky(styles);
        let align = AlignElem::alignment_in(styles).resolve(styles);

        // If the block is "rootable" it may host footnotes. In that case, we
        // defer rootness to it temporarily. We disable our own rootness to
        // prevent duplicate footnotes.
        let is_root = self.root;
        if is_root && block.rootable(styles) {
            self.root = false;
            self.regions.root = true;
        }

        // Skip directly if region is already full.
        if self.regions.is_full() {
            self.finish_region(false)?;
        }

        // Layout the block itself.
        let fragment = block.layout(
            self.engine,
            self.locator.next(&block.span()),
            styles,
            self.regions,
        )?;

        let mut notes = Vec::new();
        for (i, mut frame) in fragment.into_iter().enumerate() {
            // Find footnotes in the frame.
            if self.root {
                collect_footnotes(&mut notes, &frame);
            }

            if i > 0 {
                self.finish_region(false)?;
            }

            self.drain_tag(&mut frame);
            frame.post_process(styles);
            self.handle_item(FlowItem::Frame { frame, align, sticky, movable: false })?;
        }

        self.try_handle_footnotes(notes)?;

        self.root = is_root;
        self.regions.root = false;
        self.last_was_par = false;

        Ok(())
    }

    /// Layout a placed element.
    fn handle_place(
        &mut self,
        placed: &'a Packed<PlaceElem>,
        styles: StyleChain,
    ) -> SourceResult<()> {
        // Fetch properties.
        let float = placed.float(styles);
        let clearance = placed.clearance(styles);
        let alignment = placed.alignment(styles);
        let delta = Axes::new(placed.dx(styles), placed.dy(styles)).resolve(styles);

        let x_align = alignment.map_or(FixedAlignment::Center, |align| {
            align.x().unwrap_or_default().resolve(styles)
        });
        let y_align = alignment.map(|align| align.y().map(|y| y.resolve(styles)));

        let mut frame = placed.layout(
            self.engine,
            self.locator.next(&placed.span()),
            styles,
            self.regions.base(),
        )?;

        frame.post_process(styles);

        self.handle_item(FlowItem::Placed {
            frame,
            x_align,
            y_align,
            delta,
            float,
            clearance,
        })
    }

    /// Lays out all floating elements before continuing with other content.
    fn handle_flush(&mut self, _: &'a Packed<FlushElem>) -> SourceResult<()> {
        for item in std::mem::take(&mut self.pending_floats) {
            self.handle_item(item)?;
        }
        while !self.pending_floats.is_empty() {
            self.finish_region(false)?;
        }
        Ok(())
    }

    /// Layout a finished frame.
    fn handle_item(&mut self, mut item: FlowItem) -> SourceResult<()> {
        match item {
            FlowItem::Absolute(v, weak) => {
                if weak
                    && !self
                        .items
                        .iter()
                        .any(|item| matches!(item, FlowItem::Frame { .. },))
                {
                    return Ok(());
                }
                self.regions.size.y -= v
            }
            FlowItem::Fractional(..) => {}
            FlowItem::Frame { ref frame, movable, .. } => {
                let height = frame.height();
                while !self.regions.size.y.fits(height) && !self.regions.in_last() {
                    self.finish_region(false)?;
                }

                let in_last = self.regions.in_last();
                self.regions.size.y -= height;
                if self.root && movable {
                    let mut notes = Vec::new();
                    collect_footnotes(&mut notes, frame);
                    self.items.push(item);

                    // When we are already in_last, we can directly force the
                    // footnotes.
                    if !self.handle_footnotes(&mut notes, true, in_last)? {
                        let item = self.items.pop();
                        self.finish_region(false)?;
                        self.items.extend(item);
                        self.regions.size.y -= height;
                        self.handle_footnotes(&mut notes, true, true)?;
                    }
                    return Ok(());
                }
            }
            FlowItem::Placed { float: false, .. } => {}
            FlowItem::Placed {
                ref mut frame,
                ref mut y_align,
                float: true,
                clearance,
                ..
            } => {
                // If there is a queued float in front or if the float doesn't
                // fit, queue it for the next region.
                if !self.pending_floats.is_empty()
                    || (!self.regions.size.y.fits(frame.height() + clearance)
                        && !self.regions.in_last())
                {
                    self.pending_floats.push(item);
                    return Ok(());
                }

                // Select the closer placement, top or bottom.
                if y_align.is_auto() {
                    let ratio = (self.regions.size.y
                        - (frame.height() + clearance) / 2.0)
                        / self.regions.full;
                    let better_align = if ratio <= 0.5 {
                        FixedAlignment::End
                    } else {
                        FixedAlignment::Start
                    };
                    *y_align = Smart::Custom(Some(better_align));
                }

                // Add some clearance so that the float doesn't touch the main
                // content.
                frame.size_mut().y += clearance;
                if *y_align == Smart::Custom(Some(FixedAlignment::End)) {
                    frame.translate(Point::with_y(clearance));
                }

                self.regions.size.y -= frame.height();

                // Find footnotes in the frame.
                if self.root {
                    let mut notes = vec![];
                    collect_footnotes(&mut notes, frame);
                    self.try_handle_footnotes(notes)?;
                }
            }
            FlowItem::Footnote(_) => {}
        }

        self.items.push(item);
        Ok(())
    }

    /// Attach currently pending metadata to the frame.
    fn drain_tag(&mut self, frame: &mut Frame) {
        if !self.pending_tags.is_empty() && !frame.is_empty() {
            frame.prepend_multiple(
                self.pending_tags
                    .drain(..)
                    .map(|tag| (Point::zero(), FrameItem::Tag(tag.clone()))),
            );
        }
    }

    /// Finisht the region, migrating all sticky items to the next one.
    ///
    /// Returns whether we migrated into a last region.
    fn finish_region_with_migration(&mut self) -> SourceResult<bool> {
        // Find the suffix of sticky items.
        let mut sticky = self.items.len();
        for (i, item) in self.items.iter().enumerate().rev() {
            match *item {
                FlowItem::Absolute(_, _) => {}
                FlowItem::Frame { sticky: true, .. } => sticky = i,
                _ => break,
            }
        }

        let carry: Vec<_> = self.items.drain(sticky..).collect();
        self.finish_region(false)?;

        let in_last = self.regions.in_last();
        for item in carry {
            self.handle_item(item)?;
        }

        Ok(in_last)
    }

    /// Finish the frame for one region.
    ///
    /// Set `force` to `true` to allow creating a frame for out-of-flow elements
    /// only (this is used to force the creation of a frame in case the
    /// remaining elements are all out-of-flow).
    fn finish_region(&mut self, force: bool) -> SourceResult<()> {
        // Early return if we don't have any relevant items.
        if !force
            && !self.items.is_empty()
            && self.items.iter().all(FlowItem::is_out_of_flow)
        {
            self.finished.push(Frame::soft(self.initial));
            self.regions.next();
            self.initial = self.regions.size;
            return Ok(());
        }

        // Trim weak spacing.
        while self
            .items
            .last()
            .is_some_and(|item| matches!(item, FlowItem::Absolute(_, true)))
        {
            self.items.pop();
        }

        // Determine the used size.
        let mut fr = Fr::zero();
        let mut used = Size::zero();
        let mut footnote_height = Abs::zero();
        let mut float_top_height = Abs::zero();
        let mut float_bottom_height = Abs::zero();
        let mut first_footnote = true;
        for item in &self.items {
            match item {
                FlowItem::Absolute(v, _) => used.y += *v,
                FlowItem::Fractional(v) => fr += *v,
                FlowItem::Frame { frame, .. } => {
                    used.y += frame.height();
                    used.x.set_max(frame.width());
                }
                FlowItem::Placed { float: false, .. } => {}
                FlowItem::Placed { frame, float: true, y_align, .. } => match y_align {
                    Smart::Custom(Some(FixedAlignment::Start)) => {
                        float_top_height += frame.height()
                    }
                    Smart::Custom(Some(FixedAlignment::End)) => {
                        float_bottom_height += frame.height()
                    }
                    _ => {}
                },
                FlowItem::Footnote(frame) => {
                    footnote_height += frame.height();
                    if !first_footnote {
                        footnote_height += self.footnote_config.gap;
                    }
                    first_footnote = false;
                    used.x.set_max(frame.width());
                }
            }
        }
        used.y += footnote_height + float_top_height + float_bottom_height;

        // Determine the size of the flow in this region depending on whether
        // the region expands. Also account for fractional spacing and
        // footnotes.
        let mut size = self.expand.select(self.initial, used).min(self.initial);
        if (fr.get() > 0.0 || self.has_footnotes) && self.initial.y.is_finite() {
            size.y = self.initial.y;
        }

        if !self.regions.size.x.is_finite() && self.expand.x {
            bail!(self.flow.span(), "cannot expand into infinite width");
        }
        if !self.regions.size.y.is_finite() && self.expand.y {
            bail!(self.flow.span(), "cannot expand into infinite height");
        }

        let mut output = Frame::soft(size);
        let mut ruler = FixedAlignment::Start;
        let mut float_top_offset = Abs::zero();
        let mut offset = float_top_height;
        let mut float_bottom_offset = Abs::zero();
        let mut footnote_offset = Abs::zero();

        // Place all frames.
        for item in self.items.drain(..) {
            match item {
                FlowItem::Absolute(v, _) => {
                    offset += v;
                }
                FlowItem::Fractional(v) => {
                    let remaining = self.initial.y - used.y;
                    let length = v.share(fr, remaining);
                    offset += length;
                }
                FlowItem::Frame { frame, align, .. } => {
                    ruler = ruler.max(align.y);
                    let x = align.x.position(size.x - frame.width());
                    let y = offset + ruler.position(size.y - used.y);
                    let pos = Point::new(x, y);
                    offset += frame.height();
                    output.push_frame(pos, frame);
                }
                FlowItem::Placed { frame, x_align, y_align, delta, float, .. } => {
                    let x = x_align.position(size.x - frame.width());
                    let y = if float {
                        match y_align {
                            Smart::Custom(Some(FixedAlignment::Start)) => {
                                let y = float_top_offset;
                                float_top_offset += frame.height();
                                y
                            }
                            Smart::Custom(Some(FixedAlignment::End)) => {
                                let y = size.y - footnote_height - float_bottom_height
                                    + float_bottom_offset;
                                float_bottom_offset += frame.height();
                                y
                            }
                            _ => unreachable!("float must be y aligned"),
                        }
                    } else {
                        match y_align {
                            Smart::Custom(Some(align)) => {
                                align.position(size.y - frame.height())
                            }
                            _ => offset + ruler.position(size.y - used.y),
                        }
                    };

                    let pos = Point::new(x, y)
                        + delta.zip_map(size, Rel::relative_to).to_point();

                    output.push_frame(pos, frame);
                }
                FlowItem::Footnote(frame) => {
                    let y = size.y - footnote_height + footnote_offset;
                    footnote_offset += frame.height() + self.footnote_config.gap;
                    output.push_frame(Point::with_y(y), frame);
                }
            }
        }

        if force && !self.pending_tags.is_empty() {
            let pos = Point::with_y(offset);
            output.push_multiple(
                self.pending_tags
                    .drain(..)
                    .map(|tag| (pos, FrameItem::Tag(tag.clone()))),
            );
        }

        // Advance to the next region.
        self.finished.push(output);
        self.regions.next();
        self.initial = self.regions.size;
        self.has_footnotes = false;

        // Try to place floats into the next region.
        for item in std::mem::take(&mut self.pending_floats) {
            self.handle_item(item)?;
        }

        Ok(())
    }

    /// Finish layouting and return the resulting fragment.
    fn finish(mut self) -> SourceResult<Fragment> {
        if self.expand.y {
            while !self.regions.backlog.is_empty() {
                self.finish_region(true)?;
            }
        }

        self.finish_region(true)?;
        while !self.items.is_empty() {
            self.finish_region(true)?;
        }

        Ok(Fragment::frames(self.finished))
    }

    /// Tries to process all footnotes in the frame, placing them
    /// in the next region if they could not be placed in the current
    /// one.
    fn try_handle_footnotes(
        &mut self,
        mut notes: Vec<Packed<FootnoteElem>>,
    ) -> SourceResult<()> {
        // When we are already in_last, we can directly force the
        // footnotes.
        if self.root
            && !self.handle_footnotes(&mut notes, false, self.regions.in_last())?
        {
            self.finish_region(false)?;
            self.handle_footnotes(&mut notes, false, true)?;
        }
        Ok(())
    }

    /// Processes all footnotes in the frame.
    ///
    /// Returns true if the footnote entries fit in the allotted
    /// regions.
    fn handle_footnotes(
        &mut self,
        notes: &mut Vec<Packed<FootnoteElem>>,
        movable: bool,
        force: bool,
    ) -> SourceResult<bool> {
        let prev_notes_len = notes.len();
        let prev_items_len = self.items.len();
        let prev_size = self.regions.size;
        let prev_has_footnotes = self.has_footnotes;

        // Process footnotes one at a time.
        let mut k = 0;
        while k < notes.len() {
            if notes[k].is_ref() {
                k += 1;
                continue;
            }

            if !self.has_footnotes {
                self.layout_footnote_separator()?;
            }

            self.regions.size.y -= self.footnote_config.gap;
            let frames = layout_fragment(
                self.engine,
                &FootnoteEntry::new(notes[k].clone()).pack(),
                Locator::synthesize(notes[k].location().unwrap()),
                *self.styles,
                self.regions.with_root(false),
            )?
            .into_frames();

            // If the entries didn't fit, abort (to keep footnote and entry
            // together).
            if !force
                && (k == 0 || movable)
                && frames.first().is_some_and(Frame::is_empty)
            {
                // Undo everything.
                notes.truncate(prev_notes_len);
                self.items.truncate(prev_items_len);
                self.regions.size = prev_size;
                self.has_footnotes = prev_has_footnotes;
                return Ok(false);
            }

            let prev = notes.len();
            for (i, frame) in frames.into_iter().enumerate() {
                collect_footnotes(notes, &frame);
                if i > 0 {
                    self.finish_region(false)?;
                    self.layout_footnote_separator()?;
                    self.regions.size.y -= self.footnote_config.gap;
                }
                self.regions.size.y -= frame.height();
                self.items.push(FlowItem::Footnote(frame));
            }

            k += 1;

            // Process the nested notes before dealing with further top-level
            // notes.
            let nested = notes.len() - prev;
            if nested > 0 {
                notes[k..].rotate_right(nested);
            }
        }

        Ok(true)
    }

    /// Layout and save the footnote separator, typically a line.
    fn layout_footnote_separator(&mut self) -> SourceResult<()> {
        let expand = Axes::new(self.regions.expand.x, false);
        let pod = Region::new(self.regions.base(), expand);
        let separator = &self.footnote_config.separator;

        // FIXME: Shouldn't use `root()` here.
        let mut frame =
            layout_frame(self.engine, separator, Locator::root(), *self.styles, pod)?;
        frame.size_mut().y += self.footnote_config.clearance;
        frame.translate(Point::with_y(self.footnote_config.clearance));

        self.has_footnotes = true;
        self.regions.size.y -= frame.height();
        self.items.push(FlowItem::Footnote(frame));

        Ok(())
    }
}

/// Collect all footnotes in a frame.
fn collect_footnotes(notes: &mut Vec<Packed<FootnoteElem>>, frame: &Frame) {
    for (_, item) in frame.items() {
        match item {
            FrameItem::Group(group) => collect_footnotes(notes, &group.frame),
            FrameItem::Tag(tag)
                if !notes.iter().any(|note| note.location() == tag.elem.location()) =>
            {
                let Some(footnote) = tag.elem.to_packed::<FootnoteElem>() else {
                    continue;
                };
                notes.push(footnote.clone());
            }
            _ => {}
        }
    }
}
