use crate::locator::{new_locator, FontDataHandle, FontLocator, FontLocatorSelection};
use crate::rasterizer::{new_rasterizer, FontRasterizer};
use crate::shaper::{new_shaper, FontShaper, FontShaperSelection};
use anyhow::{anyhow, Error};
use config::{configuration, ConfigHandle, FontRasterizerSelection, TextStyle};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use wezterm_term::CellAttributes;

mod hbwrap;

pub mod ftwrap;
pub mod locator;
pub mod parser;
pub mod rasterizer;
pub mod shaper;
pub mod units;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod fcwrap;

pub use crate::rasterizer::RasterizedGlyph;
pub use crate::shaper::{FallbackIdx, FontMetrics, GlyphInfo};

pub struct LoadedFont {
    rasterizers: Vec<RefCell<Option<Box<dyn FontRasterizer>>>>,
    handles: Vec<FontDataHandle>,
    shaper: Box<dyn FontShaper>,
    metrics: FontMetrics,
    font_size: f64,
    dpi: u32,
}

impl LoadedFont {
    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub fn shape(&self, text: &str) -> anyhow::Result<Vec<GlyphInfo>> {
        self.shaper.shape(text, self.font_size, self.dpi)
    }

    pub fn metrics_for_idx(&self, font_idx: usize) -> anyhow::Result<FontMetrics> {
        self.shaper
            .metrics_for_idx(font_idx, self.font_size, self.dpi)
    }

    pub fn rasterize_glyph(
        &self,
        glyph_pos: u32,
        fallback: FallbackIdx,
    ) -> anyhow::Result<RasterizedGlyph> {
        let cell = self
            .rasterizers
            .get(fallback)
            .ok_or_else(|| anyhow!("no such fallback index: {}", fallback))?;
        let mut opt_raster = cell.borrow_mut();
        if opt_raster.is_none() {
            let raster = new_rasterizer(
                FontRasterizerSelection::get_default(),
                &self.handles[fallback],
            )?;
            opt_raster.replace(raster);
        }

        opt_raster
            .as_ref()
            .unwrap()
            .rasterize_glyph(glyph_pos, self.font_size, self.dpi)
    }
}

/// Matches and loads fonts for a given input style
pub struct FontConfiguration {
    fonts: RefCell<HashMap<TextStyle, Rc<LoadedFont>>>,
    metrics: RefCell<Option<FontMetrics>>,
    dpi_scale: RefCell<f64>,
    font_scale: RefCell<f64>,
    config_generation: RefCell<usize>,
    locator: Box<dyn FontLocator>,
}

impl FontConfiguration {
    /// Create a new empty configuration
    pub fn new() -> Self {
        let locator = new_locator(FontLocatorSelection::get_default());
        Self {
            fonts: RefCell::new(HashMap::new()),
            locator,
            metrics: RefCell::new(None),
            font_scale: RefCell::new(1.0),
            dpi_scale: RefCell::new(1.0),
            config_generation: RefCell::new(configuration().generation()),
        }
    }

    /// Given a text style, load (with caching) the font that best
    /// matches according to the fontconfig pattern.
    pub fn resolve_font(&self, style: &TextStyle) -> anyhow::Result<Rc<LoadedFont>> {
        let mut fonts = self.fonts.borrow_mut();

        let config = configuration();
        let current_generation = config.generation();
        if current_generation != *self.config_generation.borrow() {
            // Config was reloaded, invalidate our caches
            fonts.clear();
            self.metrics.borrow_mut().take();
            *self.config_generation.borrow_mut() = current_generation;
        }

        if let Some(entry) = fonts.get(style) {
            return Ok(Rc::clone(entry));
        }

        let attributes = style.font_with_fallback();
        let preferred_attributes = attributes
            .iter()
            .filter(|a| !a.is_fallback)
            .map(|a| a.clone())
            .collect::<Vec<_>>();
        let fallback_attributes = attributes
            .iter()
            .filter(|a| a.is_fallback)
            .map(|a| a.clone())
            .collect::<Vec<_>>();
        let mut loaded = HashSet::new();

        let mut handles =
            parser::ParsedFont::load_fonts(&config, &preferred_attributes, &mut loaded)?;
        handles.append(
            &mut self
                .locator
                .load_fonts(&preferred_attributes, &mut loaded)?,
        );
        handles.append(&mut parser::ParsedFont::load_built_in_fonts(
            &preferred_attributes,
            &mut loaded,
        )?);
        handles.append(&mut parser::ParsedFont::load_fonts(
            &config,
            &fallback_attributes,
            &mut loaded,
        )?);
        handles.append(&mut self.locator.load_fonts(&fallback_attributes, &mut loaded)?);
        handles.append(&mut parser::ParsedFont::load_built_in_fonts(
            &fallback_attributes,
            &mut loaded,
        )?);

        for attr in &attributes {
            if !attr.is_fallback && !loaded.contains(attr) {
                let styled_extra = if attr.bold || attr.italic {
                    ". A bold or italic variant of the font was requested; \
                    TrueType and OpenType fonts don't have an automatic way to \
                    produce these font variants, so a separate font file containing \
                    the bold or italic variant must be installed"
                } else {
                    ""
                };

                let is_primary = config.font.font.iter().any(|a| a == attr);
                let derived_from_primary = config.font.font.iter().any(|a| a.family == attr.family);

                let explanation = if is_primary {
                    // This is the primary font selection
                    format!(
                        "Unable to load a font specified by your font={} configuration",
                        attr
                    )
                } else if derived_from_primary {
                    // it came from font_rules and may have been derived from
                    // their primary font (we can't know for sure)
                    format!(
                        "Unable to load a font matching one of your font_rules: {}. \
                        Note that wezterm will synthesize font_rules to select bold \
                        and italic fonts based on your primary font configuration",
                        attr
                    )
                } else {
                    format!(
                        "Unable to load a font matching one of your font_rules: {}",
                        attr
                    )
                };

                mux::connui::show_configuration_error_message(&format!(
                    "{}. Fallback(s) are being used instead, and the terminal \
                    may not render as intended{}. See \
                    https://wezfurlong.org/wezterm/config/fonts.html for more information",
                    explanation, styled_extra
                ));
            }
        }

        let mut rasterizers = vec![];
        for _ in &handles {
            rasterizers.push(RefCell::new(None));
        }
        let shaper = new_shaper(FontShaperSelection::get_default(), &handles)?;

        let config = configuration();
        let font_size = config.font_size * *self.font_scale.borrow();
        let dpi = *self.dpi_scale.borrow() as u32 * config.dpi as u32;
        let metrics = shaper.metrics(font_size, dpi)?;

        let loaded = Rc::new(LoadedFont {
            rasterizers,
            handles,
            shaper,
            metrics,
            font_size,
            dpi,
        });

        fonts.insert(style.clone(), Rc::clone(&loaded));

        Ok(loaded)
    }

    pub fn change_scaling(&self, font_scale: f64, dpi_scale: f64) {
        *self.dpi_scale.borrow_mut() = dpi_scale;
        *self.font_scale.borrow_mut() = font_scale;
        self.fonts.borrow_mut().clear();
        self.metrics.borrow_mut().take();
    }

    /// Returns the baseline font specified in the configuration
    pub fn default_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.resolve_font(&configuration().font)
    }

    pub fn get_font_scale(&self) -> f64 {
        *self.font_scale.borrow()
    }

    pub fn default_font_metrics(&self) -> Result<FontMetrics, Error> {
        {
            let metrics = self.metrics.borrow();
            if let Some(metrics) = metrics.as_ref() {
                return Ok(*metrics);
            }
        }

        let font = self.default_font()?;
        let metrics = font.metrics();

        *self.metrics.borrow_mut() = Some(metrics);

        Ok(metrics)
    }

    /// Apply the defined font_rules from the user configuration to
    /// produce the text style that best matches the supplied input
    /// cell attributes.
    pub fn match_style<'a>(
        &self,
        config: &'a ConfigHandle,
        attrs: &CellAttributes,
    ) -> &'a TextStyle {
        // a little macro to avoid boilerplate for matching the rules.
        // If the rule doesn't specify a value for an attribute then
        // it will implicitly match.  If it specifies an attribute
        // then it has to have the same value as that in the input attrs.
        macro_rules! attr_match {
            ($ident:ident, $rule:expr) => {
                if let Some($ident) = $rule.$ident {
                    if $ident != attrs.$ident() {
                        // Does not match
                        continue;
                    }
                }
                // matches so far...
            };
        };

        for rule in &config.font_rules {
            attr_match!(intensity, &rule);
            attr_match!(underline, &rule);
            attr_match!(italic, &rule);
            attr_match!(blink, &rule);
            attr_match!(reverse, &rule);
            attr_match!(strikethrough, &rule);
            attr_match!(invisible, &rule);

            // If we get here, then none of the rules didn't match,
            // so we therefore assume that it did match overall.
            return &rule.font;
        }
        &config.font
    }
}
