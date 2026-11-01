//! A bitmap Core Graphics draws into, and the image a layer is given.
//!
//! The ghost cursor is the one thing left that is drawn this way: it is a mark a few dozen pixels across,
//! drawn when the pen moves and handed to a window of its own (see [`super::ghost`]). The canvas itself is
//! Metal geometry and has no bitmap at all.

use core_graphics::base::CGFloat;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::geometry::CGAffineTransform;
use core_graphics::image::CGImage;

/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`: premultiplied BGRA, which is the layout
/// a layer's `contents` and a Metal texture both take.
const BGRA: u32 = (2 << 12) | 2;

/// One bitmap and the context that draws into it.
pub struct Bitmap {
    pub context: CGContext,
    pub width: usize,
    pub height: usize,
}

impl Bitmap {
    pub fn new(width: usize, height: usize) -> Option<Self> {
        let space = CGColorSpace::create_device_rgb();
        let context =
            CGContext::create_bitmap_context(None, width, height, 8, width * 4, &space, BGRA);

        // Core Graphics draws with y up; everything above this module measures downwards from the top, and
        // so does the screen. One flip at the top of the bitmap makes every coordinate the app's own.
        context.concat_ctm(CGAffineTransform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            tx: 0.0,
            ty: height as CGFloat,
        });

        Some(Bitmap {
            context,
            width,
            height,
        })
    }

    pub fn image(&self) -> Option<CGImage> {
        self.context.create_image()
    }
}
