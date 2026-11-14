use std::ffi::c_void;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Point,
}

impl Rect {
    pub fn min_x(&self) -> f64 {
        self.origin.x
    }
    pub fn min_y(&self) -> f64 {
        self.origin.y
    }
    pub fn max_x(&self) -> f64 {
        self.origin.x + self.size.x
    }
    pub fn max_y(&self) -> f64 {
        self.origin.y + self.size.y
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.min_x() && point.x < self.max_x() && point.y >= self.min_y() && point.y < self.max_y()
    }

    pub fn clamp(&self, point: Point) -> Point {
        Point {
            x: point.x.clamp(self.min_x(), self.max_x() - 1.0),
            y: point.y.clamp(self.min_y(), self.max_y() - 1.0),
        }
    }

    pub fn distance(&self, point: Point) -> f64 {
        let clamped = self.clamp(point);
        (clamped.x - point.x).hypot(clamped.y - point.y)
    }

    pub fn center(&self) -> Point {
        Point { x: self.origin.x + self.size.x / 2.0, y: self.origin.y + self.size.y / 2.0 }
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> Point;
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGDisplayIsBuiltin(display: u32) -> i32;
    fn CGDisplayMirrorsDisplay(display: u32) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

pub fn cursor() -> Option<Point> {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some(point)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Display {
    pub bounds: Rect,
    pub builtin: bool,
}

/// Every non-mirroring display, in global points with the origin at the main display's top left.
/// Sleep state is not consulted: outside the GUI session macOS reports every display as asleep,
/// and the first injected movement wakes a sleeping one anyway.
pub fn online() -> Vec<Display> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    if unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize]
        .iter()
        .filter(|id| unsafe { CGDisplayMirrorsDisplay(**id) == 0 })
        .map(|id| Display {
            bounds: unsafe { CGDisplayBounds(*id) },
            builtin: unsafe { CGDisplayIsBuiltin(*id) != 0 },
        })
        .filter(|display| display.bounds.size.x > 0.0 && display.bounds.size.y > 0.0)
        .collect()
}
