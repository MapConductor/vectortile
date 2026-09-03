import CryptoKit
import XCTest

import MapConductorVectorTile

/// Runs the Rust renderer on the simulator against the MapLibre demo style and
/// a real vector tile, both bundled so the test needs no network.
final class VectorTileRendererTests: XCTestCase {

    private var styleJSON: String!
    private var tileData: Data!
    private var renderer: VectorTileRenderer!

    override func setUpWithError() throws {
        let bundle = Bundle.module
        let styleURL = try XCTUnwrap(bundle.url(forResource: "demo-style", withExtension: "json"))
        let tileURL = try XCTUnwrap(bundle.url(forResource: "tile-0-0-0", withExtension: "pbf"))
        styleJSON = try String(contentsOf: styleURL, encoding: .utf8)
        tileData = try Data(contentsOf: tileURL)
        renderer = try VectorTileRenderer(styleJSON: styleJSON)
    }

    override func tearDown() {
        renderer?.close()
        renderer = nil
    }

    func testPlansTheSourceTilesForATargetTile() throws {
        let plan = try JSONSerialization.jsonObject(
            with: Data(try renderer.plan(z: 0, x: 0, y: 0).utf8)
        ) as? [[String: Any]]
        let entries = try XCTUnwrap(plan)
        XCTAssertEqual(entries.count, 1)
        XCTAssertEqual(entries[0]["sourceId"] as? String, "maplibre")
        XCTAssertTrue((entries[0]["url"] as? String ?? "").hasSuffix("/0/0/0.pbf"))
    }

    func testOverzoomsPastTheSourceMaxzoom() throws {
        // The demo source stops at z6, so z8 must fall back to its ancestor.
        let plan = try JSONSerialization.jsonObject(
            with: Data(try renderer.plan(z: 8, x: 142, y: 99).utf8)
        ) as? [[String: Any]]
        let entry = try XCTUnwrap(plan?.first)
        XCTAssertEqual(entry["z"] as? Int, 6)
        XCTAssertEqual(entry["x"] as? Int, 35)
        XCTAssertEqual(entry["y"] as? Int, 24)
        XCTAssertEqual(entry["scale"] as? Double, 4.0)
    }

    func testRendersARealTileToADecodablePNG() throws {
        let started = CFAbsoluteTimeGetCurrent()
        let png = try renderer.render(z: 0, x: 0, y: 0, tileSize: 512, tiles: [tileData])
        let elapsedMs = (CFAbsoluteTimeGetCurrent() - started) * 1000

        XCTAssertGreaterThan(png.count, 10_000)

        let image = try XCTUnwrap(UIImage(data: png)?.cgImage)
        XCTAssertEqual(image.width, 512)
        XCTAssertEqual(image.height, 512)

        // The whole point of a shared Rust core is that every platform draws the
        // same pixels. Hashing the PNG lets the platforms be compared exactly.
        let sha = SHA256.hash(data: png).map { String(format: "%02x", $0) }.joined()
        print("IOS_RENDER_MS=\(elapsedMs)")
        print("IOS_PNG_BYTES=\(png.count)")
        print("IOS_PNG_SHA256=\(sha)")
    }

    func testWarmRenderTiming() throws {
        var samples: [Double] = []
        for _ in 0..<10 {
            let started = CFAbsoluteTimeGetCurrent()
            _ = try renderer.render(z: 0, x: 0, y: 0, tileSize: 512, tiles: [tileData])
            samples.append((CFAbsoluteTimeGetCurrent() - started) * 1000)
        }
        samples.sort()
        print("IOS_WARM_MEDIAN_MS=\(samples[samples.count / 2]) min=\(samples[0]) max=\(samples[samples.count - 1])")
        XCTAssertLessThan(samples[samples.count / 2], 5_000)
    }

    func testRestylingChangesPixelsWithoutNewTileData() throws {
        let before = try renderer.render(z: 0, x: 0, y: 0, tileSize: 256, tiles: [tileData])

        let flat = """
        {"version":8,"sources":{},"layers":[
          {"id":"bg","type":"background","paint":{"background-color":"#ff0000"}}
        ]}
        """
        try renderer.setStyle(flat)

        // Same source bytes in, different pixels out.
        let after = try renderer.render(z: 0, x: 0, y: 0, tileSize: 256, tiles: [tileData])
        XCTAssertNotEqual(before, after)

        // Decode into a known RGBA layout rather than trusting the PNG's own
        // component order, and check several points: a uniform red tile would
        // pass a single-pixel assertion no matter where it sampled.
        let pixels = try Self.rgbaPixels(png: after, width: 256, height: 256)
        for (x, y) in [(8, 8), (128, 128), (200, 40), (250, 250)] {
            let offset = (y * 256 + x) * 4
            XCTAssertEqual(pixels[offset], 0xFF, "red at \(x),\(y)")
            XCTAssertEqual(pixels[offset + 1], 0x00, "green at \(x),\(y)")
            XCTAssertEqual(pixels[offset + 2], 0x00, "blue at \(x),\(y)")
        }

        // And the original style must have been anything but uniform.
        let originalPixels = try Self.rgbaPixels(png: before, width: 256, height: 256)
        var distinct = Set<UInt32>()
        for index in stride(from: 0, to: originalPixels.count, by: 4 * 16) {
            distinct.insert(
                UInt32(originalPixels[index]) << 16
                    | UInt32(originalPixels[index + 1]) << 8
                    | UInt32(originalPixels[index + 2])
            )
        }
        XCTAssertGreaterThan(distinct.count, 10)
    }

    /// Draws a PNG into a fresh RGBA8 buffer so pixel offsets are unambiguous.
    private static func rgbaPixels(png: Data, width: Int, height: Int) throws -> [UInt8] {
        let image = try XCTUnwrap(UIImage(data: png)?.cgImage)
        var buffer = [UInt8](repeating: 0, count: width * height * 4)
        let context = try XCTUnwrap(
            CGContext(
                data: &buffer,
                width: width,
                height: height,
                bitsPerComponent: 8,
                bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            )
        )
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return buffer
    }

    func testReportsUndrawnLayerTypes() throws {
        XCTAssertTrue(try renderer.unsupportedLayerTypes().contains("symbol"))
    }

    func testDiagnosesAMapboxStandardStyleInsteadOfRenderingBlank() throws {
        // Mapbox v3 keeps the whole basemap behind an import. Nothing here can
        // follow that, and without a diagnostic the result is a blank tile with
        // no explanation.
        let mapboxStandard = """
        {"version":8,
         "imports":[{"id":"basemap","url":"mapbox://styles/mapbox/standard"}],
         "sources":{},
         "layers":[{"id":"overlay","type":"fill","source":"custom",
                    "source-layer":"zones","slot":"middle"}]}
        """
        let subject = try VectorTileRenderer(styleJSON: mapboxStandard)
        defer { subject.close() }

        let messages = try subject.diagnostics()
        XCTAssertTrue(messages.contains { $0.contains("imports") }, "\(messages)")
        XCTAssertTrue(messages.contains { $0.contains("does not define") }, "\(messages)")
    }

    func testDiagnosesASourceURLThatCannotBeFetched() throws {
        // A classic Mapbox v8 style parses fine; only its mapbox:// source URL
        // needs translating host-side, where the access token lives.
        let mapboxClassic = """
        {"version":8,
         "sources":{"composite":{"type":"vector",
                    "url":"mapbox://mapbox.mapbox-streets-v8"}},
         "layers":[{"id":"water","type":"fill","source":"composite",
                    "source-layer":"water","paint":{"fill-color":"#a0c8f0"}}]}
        """
        let subject = try VectorTileRenderer(styleJSON: mapboxClassic)
        defer { subject.close() }
        XCTAssertTrue(try subject.diagnostics().contains { $0.contains("mapbox:") })
    }

    func testABrokenStyleThrowsInsteadOfCrashing() {
        XCTAssertThrowsError(try VectorTileRenderer(styleJSON: "{ not json")) { error in
            guard case VectorTileError.styleRejected = error else {
                return XCTFail("expected styleRejected, got \(error)")
            }
        }
    }

    func testClosingTwiceIsSafe() throws {
        let subject = try VectorTileRenderer(styleJSON: styleJSON)
        subject.close()
        subject.close()
        XCTAssertThrowsError(try subject.plan(z: 0, x: 0, y: 0))
    }
}
