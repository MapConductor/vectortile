import XCTest

@testable import MapConductorVectorTile

/// Covers the half of the C ABI that iOS had none of: glyphs, sprites, the
/// split ground/label layers, and the credits a style's sources ask for.
///
/// Runs against the bundled MapLibre demo style and a real vector tile, so it
/// needs no network. The glyph range is a generated fixture carrying a solid
/// square per printable ASCII codepoint — enough for the renderer to place and
/// draw the demo style's country labels.
final class LabelBindingTests: XCTestCase {
    private var styleJSON: String!
    private var tileData: Data!
    private var glyphData: Data!
    private var renderer: VectorTileRenderer!

    override func setUpWithError() throws {
        let bundle = Bundle.module
        let styleURL = try XCTUnwrap(bundle.url(forResource: "demo-style", withExtension: "json"))
        let tileURL = try XCTUnwrap(bundle.url(forResource: "tile-0-0-0", withExtension: "pbf"))
        let glyphURL = try XCTUnwrap(bundle.url(forResource: "glyphs-0-255", withExtension: "pbf"))
        styleJSON = try String(contentsOf: styleURL, encoding: .utf8)
        tileData = try Data(contentsOf: tileURL)
        glyphData = try Data(contentsOf: glyphURL)
        renderer = try VectorTileRenderer(styleJSON: styleJSON)
    }

    override func tearDown() {
        renderer?.close()
        renderer = nil
    }

    /// The tile these tests draw.
    ///
    /// Not z0: the demo style gives `geolines-label` a minzoom of 1 and
    /// `countries-label` a minzoom of 2, so at z0 no symbol layer applies and
    /// there is correctly nothing to label. z2 has both, plus the ring of
    /// neighbours the label pass asks for.
    private let target = (z: UInt8(2), x: UInt32(1), y: UInt32(1))

    /// One entry per plan request, the same bundled tile answering each.
    private func tiles() throws -> [Data?] {
        let plan = try JSONSerialization.jsonObject(
            with: Data(try renderer.plan(z: target.z, x: target.x, y: target.y).utf8)
        ) as? [Any] ?? []
        return Array(repeating: tileData, count: plan.count)
    }

    // MARK: - Glyphs

    func testReportsTheStylesGlyphTemplate() throws {
        XCTAssertEqual(
            try renderer.glyphsURLTemplate(),
            "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf"
        )
    }

    /// The tile's labels name a fontstack, so the renderer must ask for the
    /// ranges covering the characters in them.
    func testAsksForTheGlyphRangesItsLabelsNeed() throws {
        let needed = try renderer.neededGlyphs(z: target.z, x: target.x, y: target.y, tiles: tiles())
        XCTAssertFalse(needed.isEmpty, "the demo style labels countries")
        let first = try XCTUnwrap(needed.first)
        XCTAssertTrue(first.contains("Open%20Sans%20Semibold") || first.contains("Open Sans Semibold"))
        XCTAssertTrue(first.hasSuffix(".pbf"))
    }

    /// Adding a range must be visible to the renderer, and must stop it asking
    /// for that range again.
    func testTakesAGlyphRangeAndStopsAskingForIt() throws {
        XCTAssertFalse(try renderer.hasGlyphs())
        XCTAssertEqual(try renderer.addGlyphs(glyphData), 95)
        XCTAssertTrue(try renderer.hasGlyphs())

        let stillNeeded = try renderer.neededGlyphs(z: target.z, x: target.x, y: target.y, tiles: tiles())
        XCTAssertFalse(
            stillNeeded.contains { $0.contains("0-255") },
            "0-255 is loaded; asking again would refetch it forever"
        )
    }

    /// A malformed range is a bad download, not a reason to take the app down.
    func testRejectsGarbageInsteadOfCrashing() {
        XCTAssertThrowsError(try renderer.addGlyphs(Data([0xff, 0xff, 0xff, 0xff])))
    }

    // MARK: - Sprites

    /// The demo style names no sprite, so there is nothing to fetch and the
    /// renderer must not claim otherwise.
    func testAsksForNoSpriteWhenTheStyleNamesNone() throws {
        XCTAssertNil(try renderer.spriteURLs())
        XCTAssertFalse(try renderer.needsSprite())
    }

    func testDerivesBothSpriteURLsFromTheStyle() throws {
        let styled = try VectorTileRenderer(
            styleJSON: """
            {"version": 8, "sprite": "https://example.com/sprite",
             "sources": {}, "layers": []}
            """
        )
        defer { styled.close() }
        let urls = try XCTUnwrap(try styled.spriteURLs(pixelRatio: 2))
        XCTAssertEqual(urls.json, "https://example.com/sprite@2x.json")
        XCTAssertEqual(urls.png, "https://example.com/sprite@2x.png")
    }

    // MARK: - The split layer

    /// Geometry is the half a GPU draws and fonts never invalidate: it must not
    /// change when glyphs arrive.
    func testGeometryIsUnchangedByGlyphsArriving() throws {
        let before = try renderer.renderGeometry(z: target.z, x: target.x, y: target.y, tiles: tiles())
        XCTAssertEqual(Array(before.prefix(4)), [0x89, 0x50, 0x4E, 0x47], "not a PNG")

        try renderer.addGlyphs(glyphData)
        let after = try renderer.renderGeometry(z: target.z, x: target.x, y: target.y, tiles: tiles())
        XCTAssertEqual(before, after, "geometry must not depend on fonts")
    }

    /// Without glyphs there is nothing to draw, and the caller is told so
    /// rather than handed a megabyte of transparent pixels to encode.
    func testAnEmptyLabelTileCarriesNoPixels() throws {
        let labels = try renderer.renderLabels(z: target.z, x: target.x, y: target.y, tileSize: 256, tiles: tiles())
        XCTAssertEqual(labels.placed, 0)
        XCTAssertTrue(labels.pixels.isEmpty)
    }

    /// With glyphs the labels place, and the buffer is exactly one tile of
    /// premultiplied RGBA.
    func testDrawsLabelsOnceItsGlyphsAreIn() throws {
        try renderer.addGlyphs(glyphData)
        let size: UInt32 = 256
        let labels = try renderer.renderLabels(z: target.z, x: target.x, y: target.y, tileSize: size, tiles: tiles())
        XCTAssertGreaterThan(labels.placed, 0, "the demo style labels countries")
        XCTAssertEqual(labels.pixels.count, Int(size) * Int(size) * 4)

        // Premultiplied: no pixel may carry more colour than alpha.
        for index in stride(from: 0, to: labels.pixels.count, by: 4) {
            let alpha = labels.pixels[index + 3]
            XCTAssertLessThanOrEqual(labels.pixels[index], alpha, "colour exceeds alpha at \(index)")
        }
        XCTAssertTrue(labels.pixels.contains { $0 != 0 }, "the tile is blank")
    }

    /// The other label entry point paints over pixels the caller already has,
    /// which is what a Metal readback gives.
    func testDrawsLabelsOverAReadback() throws {
        try renderer.addGlyphs(glyphData)
        let size = 256
        var readback = Data(repeating: 0, count: size * size * 4)
        for index in stride(from: 0, to: readback.count, by: 4) {
            readback[index] = 255
            readback[index + 3] = 255
        }
        let before = readback

        let placed = try renderer.drawLabels(
            z: target.z, x: target.x, y: target.y, tileSize: UInt32(size), rgba: &readback, tiles: tiles()
        )
        XCTAssertGreaterThan(placed, 0)
        XCTAssertNotEqual(before, readback, "nothing was drawn onto the readback")
    }

    // MARK: - Credits

    /// A style is data under someone's licence; the credit has to reach Swift
    /// with its HTML intact, because it is a link to that licence.
    func testCarriesTheCreditOutOfTheStyle() throws {
        let credited = try VectorTileRenderer(
            styleJSON: """
            {"version": 8,
             "sources": {"osm": {"type": "vector",
                                 "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                                 "attribution": "&copy; <a href=\\"https://osm.org/copyright\\">OSM</a>"}},
             "layers": [{"id": "w", "type": "fill", "source": "osm", "source-layer": "water"}]}
            """
        )
        defer { credited.close() }
        XCTAssertEqual(
            try credited.attributions(),
            ["&copy; <a href=\"https://osm.org/copyright\">OSM</a>"]
        )
    }

    func testInventsNoCreditWhenTheStyleAsksForNone() throws {
        XCTAssertTrue(try renderer.attributions().isEmpty)
    }

    // MARK: - Rasteriser choice

    /// This style paints no pattern, so the GPU path is usable.
    func testSaysTheGpuPathCanDrawThisTile() throws {
        XCTAssertFalse(try renderer.needsCPU(z: target.z, tiles: tiles()))
    }
}
