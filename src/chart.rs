use iced::widget::canvas::{Path, Stroke, Fill, Text, Geometry, Cache};
use iced::{Color, Rectangle, Point, Renderer, Theme};
use crate::storage::ChartDataPoint;

pub struct TelemetryCanvas<'a> {
    cache: &'a Cache,
    data: &'a [ChartDataPoint],
}

impl<'a> TelemetryCanvas<'a> {
    pub fn new(cache: &'a Cache, data: &'a [ChartDataPoint]) -> Self {
        Self {
            cache,
            data,
        }
    }
}

impl<'a, Message> iced::widget::canvas::Program<Message> for TelemetryCanvas<'a> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry> {
        let geom = self.cache.draw(renderer, bounds.size(), |frame| {
            let padding_left = 60.0;
            let padding_right = 20.0;
            let padding_top = 20.0;
            let padding_bottom = 40.0;

            let plot_width = bounds.width - padding_left - padding_right;
            let plot_height = bounds.height - padding_top - padding_bottom;

            if plot_width <= 0.0 || plot_height <= 0.0 {
                return;
            }

            // 如果没有数据，显示提示文本
            if self.data.is_empty() {
                frame.fill_text(Text {
                    content: "[DATA] No telemetry data yet".to_string(),
                    position: Point::new(bounds.width / 2.0, bounds.height / 2.0),
                    color: Color::from_rgb(0.5, 0.5, 0.5),
                    size: 14.0.into(),
                    horizontal_alignment: iced::alignment::Horizontal::Center,
                    vertical_alignment: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
                return;
            }

            // 寻找最大 Y 值
            let max_val = self.data.iter()
                .map(|p| p.raw_val.max(p.optimized_val))
                .fold(0.0f32, |m, v| m.max(v));

            let max_y = if max_val <= 0.0 { 1000.0 } else { max_val * 1.15 };

            let n = self.data.len();

            // 1. 绘制网格线与 Y 轴刻度
            let grid_color = Color::from_rgb8(0x33, 0x41, 0x55); // #334155
            let label_color = Color::from_rgb(0.6, 0.6, 0.6);

            for j in 0..=4 {
                let ratio = j as f32 / 4.0;
                let y = bounds.height - padding_bottom - ratio * plot_height;

                // 网格横线
                let grid_line = Path::new(|builder| {
                    builder.move_to(Point::new(padding_left, y));
                    builder.line_to(Point::new(bounds.width - padding_right, y));
                });
                frame.stroke(&grid_line, Stroke {
                    style: iced::widget::canvas::stroke::Style::Solid(grid_color),
                    width: 1.0,
                    ..Default::default()
                });

                // Y 轴文本
                let y_val = ratio * max_y;
                let y_text = if y_val >= 1000.0 {
                    format!("{:.1}k", y_val / 1000.0)
                } else {
                    format!("{:.0}", y_val)
                };

                frame.fill_text(Text {
                    content: y_text,
                    position: Point::new(padding_left - 8.0, y),
                    color: label_color,
                    size: 10.0.into(),
                    horizontal_alignment: iced::alignment::Horizontal::Right,
                    vertical_alignment: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
            }

            // 2. 绘制堆叠柱状图
            let y_zero = bounds.height - padding_bottom;
            let bar_width = ((plot_width / n as f32) * 0.7).clamp(2.0, 60.0);

            for (i, p) in self.data.iter().enumerate() {
                let center_x = padding_left + (i as f32 + 0.5) * (plot_width / n as f32);
                let skyline_y = y_zero - (p.raw_val / max_y) * plot_height;
                let floor_y = y_zero - (p.optimized_val / max_y) * plot_height;

                // 绘制 Stack 1: 实际 spent 消耗 (底实绿 #00e676)
                let stack1_h = y_zero - floor_y;
                if stack1_h > 0.0 {
                    let rect1 = Path::rectangle(
                        Point::new(center_x - bar_width / 2.0, floor_y),
                        iced::Size::new(bar_width, stack1_h),
                    );
                    frame.fill(&rect1, Fill {
                        style: iced::widget::canvas::fill::Style::Solid(Color::from_rgb8(0x00, 0xE6, 0x76)),
                        ..Default::default()
                    });
                }

                // 绘制 Stack 2: saved 节省量 (顶半透绿 rgba(0, 230, 118, 0.15))
                if floor_y > skyline_y {
                    let rect2 = Path::rectangle(
                        Point::new(center_x - bar_width / 2.0, skyline_y),
                        iced::Size::new(bar_width, floor_y - skyline_y),
                    );
                    frame.fill(&rect2, Fill {
                        style: iced::widget::canvas::fill::Style::Solid(Color::from_rgba8(0x00, 0xE6, 0x76, 0.15)),
                        ..Default::default()
                    });
                }
            }

            // 3. 绘制 X 轴刻度标签
            let label_step = if n > 6 { n / 5 } else { 1 };
            for (i, p) in self.data.iter().enumerate() {
                if i % label_step == 0 || i == n - 1 {
                    let center_x = padding_left + (i as f32 + 0.5) * (plot_width / n as f32);
                    frame.fill_text(Text {
                        content: p.label.clone(),
                        position: Point::new(center_x, bounds.height - padding_bottom + 12.0),
                        color: label_color,
                        size: 10.0.into(),
                        horizontal_alignment: iced::alignment::Horizontal::Center,
                        vertical_alignment: iced::alignment::Vertical::Top,
                        ..Default::default()
                    });
                }
            }
        });

        vec![geom]
    }
}
