// Unit tests for connection module
// Tests for quantization, formatting, and precision calculation functions
// Following Single Responsibility Principle: test code is separate from production code

use app::connection::BinanceFutures;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[test]
fn test_quantize_price() {
    let price = dec!(0.2593620616072499999728690579);
    let tick = dec!(0.001);
    let result = BinanceFutures::quantize_decimal(price, tick);
    assert_eq!(result, dec!(0.259));

    let result = BinanceFutures::quantize_decimal(price, dec!(0.1));
    assert_eq!(result, dec!(0.2));
}

#[test]
fn test_quantize_qty() {
    let qty = dec!(76.4964620386307103672152152);
    let step = dec!(0.001);
    let result = BinanceFutures::quantize_decimal(qty, step);
    assert_eq!(result, dec!(76.496));
}

#[test]
fn test_format_decimal_fixed() {
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(0.123456), 3), "0.123");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(5), 0), "5");
    // format_decimal_fixed trailing zero'ları korur (precision kadar)
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(1.2000), 4), "1.2000");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(0.00000001), 8), "0.00000001");

    // Yüksek fiyatlı semboller için testler (BNBUSDC gibi)
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(950.649470), 2), "950.64");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(950.649470), 3), "950.649");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(956.370530), 2), "956.37");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(956.370530), 3), "956.370");

    // Fazla precision'ı kesme testi
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(202.129776525), 2), "202.12");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(202.129776525), 3), "202.129");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(0.08082180550260300), 4), "0.0808");
    assert_eq!(
        BinanceFutures::format_decimal_fixed(dec!(0.08082180550260300), 5),
        "0.08082"
    );

    // Integer precision testi
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(100.5), 0), "100");
    assert_eq!(BinanceFutures::format_decimal_fixed(dec!(1000), 0), "1000");
}

#[test]
fn test_scale_from_step() {
    // tick_size'dan precision hesaplama testleri
    // Note: scale_from_step is a private function, so we test it indirectly
    // through quantize_decimal which uses it internally
    let step = dec!(0.1);
    let value = dec!(1.234);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1.2)); // Should quantize to 0.1 precision

    let step = dec!(0.01);
    let value = dec!(1.234);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1.23)); // Should quantize to 0.01 precision

    let step = dec!(0.001);
    let value = dec!(1.234);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1.234)); // Should quantize to 0.001 precision

    let step = dec!(0.0001);
    let value = dec!(1.234);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1.234)); // Should quantize to 0.0001 precision

    let step = dec!(1);
    let value = dec!(1.234);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1)); // Should quantize to 1 precision

    let step = dec!(10);
    let value = dec!(123.456);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(120)); // Should quantize to 10 precision

    let step = dec!(0.000001);
    let value = dec!(1.234567);
    let result = BinanceFutures::quantize_decimal(value, step);
    assert_eq!(result, dec!(1.234567)); // Should quantize to 0.000001 precision
}

