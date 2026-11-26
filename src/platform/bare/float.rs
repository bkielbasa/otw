//! Floating point functions `std` gives f32 and f64 and `core` does not,
//! from `libm`.

pub trait FloatExt: Sized {
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn round(self) -> Self;
    fn trunc(self) -> Self;
    fn fract(self) -> Self;
    fn sqrt(self) -> Self;
    fn cbrt(self) -> Self;
    fn powf(self, n: Self) -> Self;
    fn powi(self, n: i32) -> Self;
    fn exp(self) -> Self;
    fn exp2(self) -> Self;
    fn ln(self) -> Self;
    fn log(self, base: Self) -> Self;
    fn log2(self) -> Self;
    fn log10(self) -> Self;
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn tan(self) -> Self;
    fn asin(self) -> Self;
    fn acos(self) -> Self;
    fn atan(self) -> Self;
    fn atan2(self, other: Self) -> Self;
    fn sinh(self) -> Self;
    fn cosh(self) -> Self;
    fn tanh(self) -> Self;
    fn hypot(self, other: Self) -> Self;
    fn mul_add(self, a: Self, b: Self) -> Self;
    fn rem_euclid(self, rhs: Self) -> Self;
    fn sin_cos(self) -> (Self, Self);
    fn exp_m1(self) -> Self;
    fn ln_1p(self) -> Self;
    fn asinh(self) -> Self;
    fn acosh(self) -> Self;
    fn atanh(self) -> Self;
    fn round_ties_even(self) -> Self;
}

macro_rules! float_ext {
    ($t:ty, $floor:ident, $ceil:ident, $round:ident, $trunc:ident, $sqrt:ident, $cbrt:ident, $pow:ident, $exp:ident,
     $exp2:ident, $ln:ident, $log2:ident, $log10:ident, $sin:ident, $cos:ident, $tan:ident, $asin:ident, $acos:ident,
     $atan:ident, $atan2:ident, $sinh:ident, $cosh:ident, $tanh:ident, $hypot:ident, $fma:ident, $expm1:ident, $log1p:ident,
     $asinh:ident, $acosh:ident, $atanh:ident, $rint:ident) => {
        impl FloatExt for $t {
            fn floor(self) -> Self { libm::$floor(self) }
            fn ceil(self) -> Self { libm::$ceil(self) }
            fn round(self) -> Self { libm::$round(self) }
            fn trunc(self) -> Self { libm::$trunc(self) }
            fn fract(self) -> Self { self - libm::$trunc(self) }
            fn sqrt(self) -> Self { libm::$sqrt(self) }
            fn cbrt(self) -> Self { libm::$cbrt(self) }
            fn powf(self, n: Self) -> Self { libm::$pow(self, n) }
            fn powi(self, n: i32) -> Self { libm::$pow(self, n as $t) }
            fn exp(self) -> Self { libm::$exp(self) }
            fn exp2(self) -> Self { libm::$exp2(self) }
            fn ln(self) -> Self { libm::$ln(self) }
            fn log(self, base: Self) -> Self { libm::$ln(self) / libm::$ln(base) }
            fn log2(self) -> Self { libm::$log2(self) }
            fn log10(self) -> Self { libm::$log10(self) }
            fn sin(self) -> Self { libm::$sin(self) }
            fn cos(self) -> Self { libm::$cos(self) }
            fn tan(self) -> Self { libm::$tan(self) }
            fn asin(self) -> Self { libm::$asin(self) }
            fn acos(self) -> Self { libm::$acos(self) }
            fn atan(self) -> Self { libm::$atan(self) }
            fn atan2(self, other: Self) -> Self { libm::$atan2(self, other) }
            fn sinh(self) -> Self { libm::$sinh(self) }
            fn cosh(self) -> Self { libm::$cosh(self) }
            fn tanh(self) -> Self { libm::$tanh(self) }
            fn hypot(self, other: Self) -> Self { libm::$hypot(self, other) }
            fn mul_add(self, a: Self, b: Self) -> Self { libm::$fma(self, a, b) }
            fn rem_euclid(self, rhs: Self) -> Self {
                let r = self % rhs;
                if r < 0.0 { r + if rhs < 0.0 { -rhs } else { rhs } } else { r }
            }
            fn sin_cos(self) -> (Self, Self) { (libm::$sin(self), libm::$cos(self)) }
            fn exp_m1(self) -> Self { libm::$expm1(self) }
            fn ln_1p(self) -> Self { libm::$log1p(self) }
            fn asinh(self) -> Self { libm::$asinh(self) }
            fn acosh(self) -> Self { libm::$acosh(self) }
            fn atanh(self) -> Self { libm::$atanh(self) }
            fn round_ties_even(self) -> Self { libm::$rint(self) }
        }
    };
}

float_ext!(f64, floor, ceil, round, trunc, sqrt, cbrt, pow, exp, exp2, log, log2, log10, sin, cos, tan, asin, acos,
    atan, atan2, sinh, cosh, tanh, hypot, fma, expm1, log1p, asinh, acosh, atanh, rint);
float_ext!(f32, floorf, ceilf, roundf, truncf, sqrtf, cbrtf, powf, expf, exp2f, logf, log2f, log10f, sinf, cosf, tanf,
    asinf, acosf, atanf, atan2f, sinhf, coshf, tanhf, hypotf, fmaf, expm1f, log1pf, asinhf, acoshf, atanhf, rintf);
