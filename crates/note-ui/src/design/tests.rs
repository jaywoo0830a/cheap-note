use super::*;

/// Every layout token the design system currently owns, in one iterator.
fn all_tokens() -> impl Iterator<Item = DesignUnit> {
    SPACING
        .iter()
        .copied()
        .chain(CONTROLS.iter().copied())
        .chain([CONTROL_GAP])
        .chain(LAYOUT.iter().copied())
        .chain(RADII.iter().copied())
}

/// A design unit is either on the four-unit grid or it is not a design unit at all.
#[test]
fn the_grid_refuses_off_grid_lengths() {
    assert_eq!(GRID, 4);
    assert_eq!(DesignUnit::new(4).map(DesignUnit::get), Some(4));
    assert_eq!(DesignUnit::new(0).map(DesignUnit::get), Some(0));
    assert_eq!(DesignUnit::new(5), None);
    assert_eq!(DesignUnit::new(u16::MAX), None);
}

/// No token may be off the grid: every one of them has to have an exact answer at every step.
#[test]
fn every_token_is_on_the_grid() {
    for token in all_tokens() {
        assert_eq!(
            token.get() % GRID,
            0,
            "{} is not a multiple of the {} design-unit grid",
            token.get(),
            GRID
        );
        assert_eq!(
            DesignUnit::new(token.get()),
            Some(token),
            "a token refused by its own constructor"
        );
        assert_eq!(token.grids() * GRID, token.get());
    }
}

/// The allowed scales are exactly the quarter steps from 50% to 200%, in order, and the persisted
/// index round-trips through every one of them.
#[test]
fn the_steps_are_the_seven_quarter_steps() {
    let expected: [u16; 7] = [50, 75, 100, 125, 150, 175, 200];
    assert_eq!(ScaleStep::ALL.len(), expected.len());
    assert_eq!(ScaleStep::default(), ScaleStep::P100);

    for (index, (step, percent)) in ScaleStep::ALL.iter().zip(expected).enumerate() {
        assert_eq!(step.percent(), percent);
        assert_eq!(step.index(), index);
        assert_eq!(ScaleStep::from_index(index), Some(*step));
    }

    assert_eq!(ScaleStep::from_index(expected.len()), None);
    assert_eq!(ScaleStep::from_index(usize::MAX), None);
}

/// 100% is the design's own numbers, with nothing added by the conversion.
#[test]
fn one_hundred_percent_is_identity() {
    for token in all_tokens() {
        assert_eq!(
            px(token, ScaleStep::P100).get(),
            token.get() as u32,
            "{} design units changed at 100%",
            token.get()
        );
    }
}

/// Every token at every step is exactly its percentage of the original: no rounding, no truncation,
/// and no platform-dependent floating-point arithmetic.
#[test]
fn every_step_is_an_exact_whole_pixel() {
    for token in all_tokens() {
        for step in ScaleStep::ALL {
            let scaled = px(token, step).get();

            assert_eq!(
                scaled * 100,
                token.get() as u32 * step.percent() as u32,
                "{} design units at {}% became {} px instead of an exact percentage",
                token.get(),
                step.percent(),
                scaled
            );
        }
    }
}

/// The conversion is linear: scaling a sum is the same as adding the scaled parts. This is the
/// property that keeps a row's gaps and its content from drifting apart as the scale changes.
#[test]
fn scaling_is_linear() {
    for step in ScaleStep::ALL {
        for left in all_tokens() {
            for right in all_tokens() {
                let sum = DesignUnit::new(left.get() + right.get())
                    .expect("the sum of two on-grid lengths is on the grid");

                assert_eq!(
                    px(sum, step).get(),
                    px(left, step).get() + px(right, step).get(),
                    "{} + {} design units at {}% did not add up",
                    left.get(),
                    right.get(),
                    step.percent()
                );
            }
        }
    }
}

/// A larger scale never makes a length smaller. It is not enough for the endpoints to be right: a
/// mistake in the quantum would show up as a step backwards in the middle of the range.
#[test]
fn scaling_is_monotonic() {
    for token in all_tokens() {
        let mut previous = LayoutPx::ZERO;

        for step in ScaleStep::ALL {
            let current = px(token, step);

            assert!(
                current >= previous,
                "{} design units shrank from {:?} to {:?} at {}%",
                token.get(),
                previous,
                current,
                step.percent()
            );
            previous = current;
        }
    }
}

/// A small golden table, so a change to the grid or to the step list is visible as a diff rather
/// than only as a test that still happens to pass.
#[test]
fn the_scale_table_is_what_the_design_expects() {
    let expected = [
        (ScaleStep::P50, 74, 12, 4, 8),
        (ScaleStep::P75, 111, 18, 6, 12),
        (ScaleStep::P100, 148, 24, 8, 16),
        (ScaleStep::P125, 185, 30, 10, 20),
        (ScaleStep::P150, 222, 36, 12, 24),
        (ScaleStep::P175, 259, 42, 14, 28),
        (ScaleStep::P200, 296, 48, 16, 32),
    ];

    for (step, bar, page_margin, gap, radius) in expected {
        let metrics = LayoutMetrics::at(step);

        assert_eq!(metrics.bar_height.get(), bar);
        assert_eq!(metrics.page_margin.get(), page_margin);
        assert_eq!(metrics.control_gap.get(), gap);
        assert_eq!(metrics.radius_large.get(), radius);
    }
}

/// The snapshot is nothing but the tokens at one scale: every field is checked against the same
/// conversion, so a field cannot silently be left at its 100% value.
#[test]
fn metrics_are_the_tokens_at_the_same_scale() {
    for step in ScaleStep::ALL {
        let metrics = LayoutMetrics::at(step);

        assert_eq!(metrics.scale, step);
        assert_eq!(metrics.bar_height, px(BAR_HEIGHT, step));
        assert_eq!(metrics.bar_margin, px(BAR_MARGIN, step));
        assert_eq!(metrics.page_margin, px(PAGE_MARGIN, step));
        assert_eq!(metrics.screen_margin, px(SCREEN_MARGIN, step));
        assert_eq!(metrics.foot_band, px(FOOT_BAND, step));
        assert_eq!(metrics.foot_slack, px(FOOT_SLACK, step));
        assert_eq!(metrics.status_drop, px(STATUS_DROP, step));
        assert_eq!(metrics.control_small, px(CONTROL_SMALL, step));
        assert_eq!(metrics.control_medium, px(CONTROL_MEDIUM, step));
        assert_eq!(metrics.control_large, px(CONTROL_LARGE, step));
        assert_eq!(metrics.control_gap, px(CONTROL_GAP, step));
        assert_eq!(metrics.radius_small, px(RADIUS_SMALL, step));
        assert_eq!(metrics.radius_medium, px(RADIUS_MEDIUM, step));
        assert_eq!(metrics.radius_large, px(RADIUS_LARGE, step));
        assert_eq!(metrics.space_1, px(SPACE_1, step));
        assert_eq!(metrics.space_2, px(SPACE_2, step));
        assert_eq!(metrics.space_3, px(SPACE_3, step));
        assert_eq!(metrics.space_4, px(SPACE_4, step));
        assert_eq!(metrics.space_6, px(SPACE_6, step));
        assert_eq!(metrics.space_8, px(SPACE_8, step));
        assert_eq!(metrics.space_12, px(SPACE_12, step));
        assert_eq!(metrics.space_16, px(SPACE_16, step));
    }
}

/// The default is the shipped design, not an arbitrary zero or a first enum variant.
#[test]
fn the_default_metrics_are_the_shipped_design() {
    let metrics = LayoutMetrics::default();

    assert_eq!(metrics.scale, ScaleStep::P100);
    assert_eq!(metrics.bar_height.get(), 148);
    assert_eq!(metrics.page_margin.get(), 24);
    assert_eq!(metrics.screen_margin.get(), 56);
    assert_eq!(metrics.control_medium.get(), 32);
    assert_eq!(metrics.radius_large.get(), 16);
    assert_eq!(metrics.space_2.get(), 8);
}

/// The type ramp starts at the sizes the 100% design was drawn with.
#[test]
fn the_type_ramp_starts_at_the_shipped_sizes() {
    let ramp = TypeRamp::default();

    assert_eq!(ramp.base, 13.0);
    assert_eq!(ramp.micro, 10.0);
    assert_eq!(ramp.caption, 11.0);
    assert_eq!(ramp.title, 15.0);
    assert_eq!(ramp, TypeRamp::at(ScaleStep::P100));
}

/// Every type role is exactly its 100% size times the same quarter-step. The values are exact in
/// binary floating point because every step is a quarter.
#[test]
fn every_type_step_is_the_same_proportion() {
    for step in ScaleStep::ALL {
        let factor = step.percent() as f32 / 100.0;
        let ramp = TypeRamp::at(step);

        assert_eq!(ramp.base, 13.0 * factor);
        assert_eq!(ramp.micro, 10.0 * factor);
        assert_eq!(ramp.caption, 11.0 * factor);
        assert_eq!(ramp.title, 15.0 * factor);
    }

    assert_eq!(
        TypeRamp::at(ScaleStep::P150),
        TypeRamp {
            base: 19.5,
            micro: 15.0,
            caption: 16.5,
            title: 22.5,
        }
    );
}
