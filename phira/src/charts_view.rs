use crate::{
    client::Chart,
    dir, get_data, get_data_mut,
    icons::Icons,
    page::{ChartItem, ChartType, Fader, Illustration},
    save_data,
    scene::{MP_PANEL, SongScene, render_release_to_refresh},
};
use anyhow::Result;
use macroquad::prelude::*;
use prpr::{
    core::{Tweenable, BOLD_FONT},
    ext::{semi_black, RectExt, SafeTexture, BLACK_TEXTURE},
    scene::{show_message, show_error, NextScene},
    task::Task,
    ui::{button_hit_large, DRectButton, Scroll, Ui},
};
use std::{
    ops::Range,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

pub static NEED_UPDATE: AtomicBool = AtomicBool::new(false);

const CHART_PADDING: f32 = 0.013;
const TRANSIT_TIME: f32 = 0.4;
const BACK_FADE_IN_TIME: f32 = 0.2;

pub struct ChartDisplayItem {
    chart: Option<ChartItem>,
    symbol: Option<char>,
    btn: DRectButton,

    is_selected: Option<bool>,
}

impl ChartDisplayItem {
    pub fn new(chart: Option<ChartItem>, symbol: Option<char>) -> Self {
        Self {
            chart,
            symbol,
            btn: DRectButton::new(),
            is_selected: None,
        }
    }

    pub fn from_remote(chart: &Chart) -> Self {
        Self::new(
            Some(ChartItem {
                info: chart.to_info(),
                illu: {
                    let notify: Arc<Notify> = Arc::new(Notify::new());
                    Illustration {
                        texture: (BLACK_TEXTURE.clone(), BLACK_TEXTURE.clone()),
                        notify: Arc::clone(&notify),
                        task: Some(Task::new({
                            let illu = chart.illustration.clone();
                            async move {
                                notify.notified().await;
                                Ok((illu.load_thumbnail().await?, None))
                            }
                        })),
                        loaded: Arc::default(),
                        load_time: f32::NAN,
                    }
                },
                local_path: None,
                chart_type: ChartType::Downloaded,
            }),
            if chart.stable_request {
                Some('+')
            } else if !chart.reviewed {
                Some('*')
            } else {
                None
            },
        )
    }
}

struct TransitState {
    id: u32,
    rect: Option<Rect>,
    chart: ChartItem,
    start_time: f32,
    next_scene: Option<NextScene>,
    back: bool,
    done: bool,
    delete: bool,
}

pub struct ChartsView {
    scroll: Scroll,
    fader: Fader,

    icons: Arc<Icons>,
    rank_icons: [SafeTexture; 8],

    back_fade_in: Option<(u32, f32)>,

    transit: Option<TransitState>,
    charts: Option<Vec<ChartDisplayItem>>,

    pub row_num: u32,
    pub row_height: f32,

    pub can_refresh: bool,

    pub clicked_special: bool,

    ensure_delete: Arc<AtomicBool>,

    delete_task: Option<Task<Result<usize>>>,
    pending_delete_paths: Option<Vec<String>>,
}

impl ChartsView {
    pub fn new(icons: Arc<Icons>, rank_icons: [SafeTexture; 8]) -> Self {
        Self {
            scroll: Scroll::new(),
            fader: Fader::new().with_distance(0.06),

            icons,
            rank_icons,

            back_fade_in: None,

            transit: None,
            charts: None,

            row_num: 4,
            row_height: 0.3,

            can_refresh: true,

            clicked_special: false,

            ensure_delete: Arc::new(AtomicBool::new(false)),
            delete_task: None,
            pending_delete_paths: None,
        }
    }

    fn charts_display_range(&self, content_size: (f32, f32)) -> Range<u32> {
        let sy = self.scroll.y_scroller.offset;
        let start_line = (sy / self.row_height) as u32;
        let end_line = ((sy + content_size.1) / self.row_height).ceil() as u32;
        (start_line * self.row_num)..((end_line + 1) * self.row_num)
    }

    pub fn clear(&mut self) {
        self.charts = None;
    }

    pub fn set(&mut self, t: f32, charts: Vec<ChartDisplayItem>) {
        self.charts = Some(charts);
        self.fader.sub(t);
    }

    pub fn reset_scroll(&mut self) {
        self.scroll.y_scroller.reset();
    }

    pub fn transiting(&self) -> bool {
        self.transit.is_some()
    }

    pub fn on_result(&mut self, t: f32, delete: bool) {
        if let Some(transit) = &mut self.transit {
            transit.start_time = t;
            transit.back = true;
            transit.done = false;
            transit.delete = delete;
        }
    }

    pub fn need_update(&self) -> bool {
        NEED_UPDATE.fetch_and(false, Ordering::Relaxed)
    }

    pub fn delete_charts_batch(&mut self, indices: Vec<u32>) -> Result<usize> {
        if indices.is_empty() {
            return Ok(0);
        }

        let Some(charts) = &self.charts else {
            return Ok(0);
        };

        let mut delete_paths = Vec::new();
        for &idx in &indices {
            if let Some(item) = charts.get(idx as usize) {
                if let Some(chart) = &item.chart {
                    let path = if let Some(path) = &chart.local_path {
                        path.clone()
                    } else {
                        format!("download/{}", chart.info.id.unwrap_or(0))
                    };
                    delete_paths.push(path);
                }
            }
        }

        if delete_paths.is_empty() {
            return Ok(0);
        }
        let paths_to_delete = delete_paths.clone();
        self.pending_delete_paths = Some(delete_paths);
        self.delete_task = Some(Task::new(async move {
            let mut deleted_count: usize = 0;
            let base = dir::charts()?;
            for path in paths_to_delete {
                let full = format!("{}/{path}", base);
                if std::fs::remove_dir_all(&full).is_ok() {
                    deleted_count += 1;
                } else {
                    show_error(anyhow::anyhow!(format!("Failed to delete chart at path: {full}")));
                }
            }
            Ok(deleted_count)
        }));

        Ok(self.pending_delete_paths.as_ref().map(|v| v.len()).unwrap_or(0))
    }

    pub fn get_selected_indices(&self) -> Vec<u32> {
        let Some(charts) = &self.charts else {
            return Vec::new();
        };

        charts
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| if item.is_selected == Some(true) { Some(idx as u32) } else { None })
            .collect()
    }

    pub fn process_delete_task(&mut self) -> Result<()> {
        if let Some(task) = &mut self.delete_task {
            if let Some(res) = task.take() {
                match res {
                    Ok(count) => {
                        if let Some(paths) = self.pending_delete_paths.take() {
                            let data = get_data_mut();
                            for path in paths {
                                if let Some(idx) = data.find_chart_by_path(path.as_str()) {
                                    data.charts.remove(idx);
                                }
                            }
                            if count > 0 {
                                let _ = save_data();
                                NEED_UPDATE.store(true, Ordering::SeqCst);
                                show_message(format!("Deleted {} chart(s)", count)).ok();
                            }
                        }
                    }
                    Err(e) => {
                        show_error(e);
                        self.pending_delete_paths.take();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn check_delete(&self, v: bool) {
        self.ensure_delete.store(v, Ordering::SeqCst);
    }

    pub fn touch(&mut self, touch: &Touch, t: f32, rt: f32) -> Result<bool> {
        self.touch_with_select(touch, t, rt, false, Arc::new(AtomicBool::new(false)))
    }

    pub fn touch_with_select(&mut self, touch: &Touch, t: f32, rt: f32, is_selecting: bool, ensure_delete: Arc<AtomicBool>) -> Result<bool> {
        self.ensure_delete.store(ensure_delete.load(Ordering::SeqCst), Ordering::SeqCst);
        if self.scroll.touch(touch, t) {
            return Ok(true);
        }
        if self.scroll.contains(touch) {
            if let Some(charts) = &mut self.charts {
                for (id, item) in charts.iter_mut().enumerate() {
                    if is_selecting == true && item.btn.touch(touch, t) {
                        item.is_selected = Some(item.is_selected != Some(true));
                        return Ok(true);
                    }
                    if let Some(chart) = &mut item.chart {
                        if item.btn.touch(touch, t) {
                            button_hit_large();
                            let handled_by_mp = MP_PANEL.with(|it| {
                                if let Some(panel) = it.borrow_mut().as_mut() {
                                    if panel.in_room() {
                                        if let Some(id) = chart.info.id {
                                            panel.select_chart(id);
                                            panel.show(rt);
                                        } else {
                                            use crate::mp::{mtl, L10N_LOCAL};
                                            show_message(mtl!("select-chart-local")).error();
                                        }
                                        return true;
                                    }
                                }
                                false
                            });
                            if handled_by_mp {
                                continue;
                            }
                            let download_path = chart.info.id.map(|it| format!("download/{it}"));
                            let scene = SongScene::new(
                                chart.clone(),
                                if let Some(path) = &chart.local_path {
                                    Some(path.clone())
                                } else {
                                    let path = download_path.clone().unwrap();
                                    if Path::new(&format!("{}/{path}", dir::charts()?)).exists() {
                                        Some(path)
                                    } else {
                                        None
                                    }
                                },
                                Arc::clone(&self.icons),
                                self.rank_icons.clone(),
                                get_data()
                                    .charts
                                    .iter()
                                    .find(|it| Some(&it.local_path) == download_path.as_ref())
                                    .map(|it| it.mods)
                                    .unwrap_or_default(),
                            );
                            self.transit = Some(TransitState {
                                id: id as _,
                                rect: None,
                                chart: chart.clone(),
                                start_time: t,
                                next_scene: Some(NextScene::Overlay(Box::new(scene))),
                                back: false,
                                done: false,
                                delete: false,
                            });
                            return Ok(true);
                        }
                    } else if item.btn.touch(touch, t) {
                        button_hit_large();
                        self.clicked_special = true;
                    }
                }
            }
        }
        Ok(false)
    }

    pub fn update(&mut self, t: f32) -> Result<bool> {
        self.process_delete_task()?;
        let refreshed = self.can_refresh && self.scroll.y_scroller.pulled;
        self.scroll.update(t);
        let mut do_delete = None;
        let mut do_back = None;
        let mut clear_transit = false;

        if let Some(transit) = &mut self.transit {
            transit.chart.illu.settle(t);
            if t > transit.start_time + TRANSIT_TIME {
                if transit.back {
                    if transit.delete {
                        do_delete = Some(transit.id);
                    } else {
                        do_back = Some(transit.id);
                    }
                    clear_transit = true;
                } else {
                    transit.done = true;
                }
            }
        }

        if let Some(id) = do_delete {
            if self.ensure_delete.load(Ordering::SeqCst) {
            let _ = self.delete_charts_batch(vec![id])?;
            self.check_delete(false);
            }
        }
        if let Some(id) = do_back {
            self.back_fade_in = Some((id, t));
        }
        if clear_transit {
            self.transit = None;
        }

        if let Some(charts) = &mut self.charts {
            for chart in charts {
                if let Some(chart) = &mut chart.chart {
                    chart.illu.settle(t);
                }
            }
        }

        Ok(refreshed)
    }

    pub fn render(&mut self, ui: &mut Ui, r: Rect, t: f32) {
        let content_size = (r.w, r.h);
        let range = self.charts_display_range(content_size);
        let Some(charts) = &mut self.charts else {
            let ct = r.center();
            ui.loading(ct.x, ct.y, t, WHITE, ());
            return;
        };
        if charts.is_empty() {
            let ct = r.center();
            ui.text(ttl!("list-empty")).pos(ct.x, ct.y).anchor(0.5, 0.5).no_baseline().draw();
            return;
        }
        ui.scope(|ui| {
            ui.dx(r.x);
            ui.dy(r.y);
            let off = self.scroll.y_scroller.offset;
            self.scroll.size(content_size);
            self.scroll.render(ui, |ui| {
                if self.can_refresh {
                    render_release_to_refresh(ui, r.w / 2., off);
                }
                let cw = r.w / self.row_num as f32;
                let ch = self.row_height;
                let p = CHART_PADDING;
                let r = Rect::new(p, p, cw - p * 2., ch - p * 2.);
                self.fader.reset();
                self.fader.for_sub(|f| {
                    ui.hgrids(content_size.0, ch, self.row_num, charts.len() as u32, |ui, id| {
                        if let Some(transit) = &mut self.transit {
                            if transit.id == id {
                                transit.rect = Some(ui.rect_to_global(r));
                            }
                        }
                        if !range.contains(&id) {
                            if let Some(item) = charts.get_mut(id as usize) {
                                item.btn.invalidate();
                            }
                            return;
                        }    
                        f.render(ui, t, |ui| {
                            let mut c = WHITE;

                            let item = &mut charts[id as usize];

                            item.btn.render_shadow(ui, r, t, |ui, path| {
                                if let Some(chart) = &mut item.chart {
                                    chart.illu.notify();
                                    ui.fill_path(&path, semi_black(c.a));
                                    ui.fill_path(&path, chart.illu.shading(r.feather(0.01), t));
                                    let selected = item.is_selected == Some(true);
                                    if selected {
                                        ui.fill_path(&path, Color::new(0.12, 0.35, 0.95, 0.45));
                                    }
                                    if let Some((that_id, start_time)) = &self.back_fade_in {
                                        if id == *that_id {
                                            let p = ((t - start_time) / BACK_FADE_IN_TIME).max(0.);
                                            if p > 1. {
                                                self.back_fade_in = None;
                                            } else {
                                                ui.fill_path(&path, semi_black(0.55 * (1. - p)));
                                                c.a *= p;
                                            }
                                        }
                                    }

                                    ui.fill_path(&path, (semi_black(0.4 * c.a), (0., 0.), semi_black(0.8 * c.a), (0., ch)));

                                    let info = &chart.info;
                                    let mut level = info.level.clone();
                                    if !level.contains("Lv.") {
                                        use std::fmt::Write;
                                        write!(&mut level, " Lv.{}", info.difficulty as i32).unwrap();
                                    }
                                    let mut t = ui
                                        .text(level)
                                        .pos(r.right() - 0.016, r.y + 0.016)
                                        .max_width(r.w * 2. / 3.)
                                        .anchor(1., 0.)
                                        .size(0.52 * r.w / cw)
                                        .color(c);
                                    let ms = t.measure();
                                    t.ui.fill_path(
                                        &ms.feather(0.008).rounded(0.01),
                                        Color {
                                            a: c.a * 0.7,
                                            ..t.ui.background()
                                        },
                                    );
                                    t.draw();
                                    ui.text(&info.name)
                                        .pos(r.x + 0.01, r.bottom() - 0.02)
                                        .max_width(r.w)
                                        .anchor(0., 1.)
                                        .size(0.6 * r.w / cw)
                                        .color(c)
                                        .draw();
                                    if let Some(symbol) = item.symbol {
                                        ui.text(symbol.to_string())
                                            .pos(r.x + 0.01, r.y + 0.01)
                                            .size(0.8 * r.w / cw)
                                            .color(c)
                                            .draw();
                                    }
                                } else {
                                    ui.fill_path(&path, (*self.icons.r#abstract, r));
                                    ui.fill_path(&path, semi_black(0.2));
                                    let ct = r.center();
                                    use crate::page::coll::*;
                                    ui.text(tl!("label"))
                                        .pos(ct.x, ct.y)
                                        .anchor(0.5, 0.5)
                                        .no_baseline()
                                        .size(0.7)
                                        .draw_using(&BOLD_FONT);
                                }
                            });
                        });
                    })
                })
            });
        });
    }

    pub fn render_top(&mut self, ui: &mut Ui, t: f32) {
        if let Some(transit) = &self.transit {
            if let Some(fr) = transit.rect {
                let p = ((t - transit.start_time) / TRANSIT_TIME).clamp(0., 1.);
                let p = (1. - p).powi(4);
                let p = if transit.back { p } else { 1. - p };
                let r = Rect::tween(&fr, &ui.screen_rect(), p);
                let path = r.rounded(0.02 * (1. - p));
                ui.fill_path(&path, (*transit.chart.illu.texture.1, r.feather(0.01 * (1. - p))));
                ui.fill_path(&path, semi_black(0.55));
            }
        }
    }

    pub fn next_scene(&mut self) -> Option<NextScene> {
        if let Some(transit) = &mut self.transit {
            if transit.done {
                return transit.next_scene.take();
            }
        }
        None
    }
}
