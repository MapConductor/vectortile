import CoreGraphics
import CryptoKit
import MapConductorCore
import MapConductorVectorTile
import SwiftUI
import UIKit

@main
struct VTBenchApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

/// Renders the bundled z14 Tokyo tile through the same Rust renderer the SDK
/// module ships, on real Apple hardware.
///
/// Results go to stdout so `devicectl … launch --console` can capture them,
/// and the PNG goes to Documents so it can be pulled and hashed against the
/// desktop reference — the byte-identical-output claim has covered macOS,
/// Android hardware and the iOS *simulator*; this is the first real Apple
/// device in the chain.
struct ContentView: View {
    @State private var image: UIImage?
    @State private var report = "running…"

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(report)
                    .font(.system(size: 12, design: .monospaced))
                if let image {
                    Image(uiImage: image)
                        .resizable()
                        .scaledToFit()
                        .border(Color.gray)
                }
            }
            .padding()
        }
        .onAppear { DispatchQueue.global().async { run() } }
    }

    private func run() {
        var lines: [String] = []
        func log(_ line: String) {
            lines.append(line)
            print("VTBENCH " + line)
            DispatchQueue.main.async { report = lines.joined(separator: "\n") }
        }

        do {
            guard
                let styleURL = Bundle.main.url(forResource: "style", withExtension: "json"),
                let tileURL = Bundle.main.url(forResource: "tile", withExtension: "mvt")
            else {
                log("FAILED: bundled resources missing")
                return
            }
            let styleJSON = try String(contentsOf: styleURL, encoding: .utf8)
            let tileData = try Data(contentsOf: tileURL)

            let renderer = try VectorTileRenderer(styleJSON: styleJSON)
            defer { renderer.close() }

            // Warm once; then a median over ten runs, the same protocol the
            // Android and desktop numbers used.
            _ = try renderer.render(z: 14, x: 14552, y: 6451, tileSize: 512, tiles: [tileData])
            var samples: [Double] = []
            var png = Data()
            for _ in 0..<10 {
                let started = CFAbsoluteTimeGetCurrent()
                png = try renderer.render(
                    z: 14, x: 14552, y: 6451, tileSize: 512, tiles: [tileData]
                )
                samples.append((CFAbsoluteTimeGetCurrent() - started) * 1000)
            }
            samples.sort()

            let sha = SHA256.hash(data: png).map { String(format: "%02x", $0) }.joined()
            log("device \(UIDevice.current.model) \(UIDevice.current.systemVersion)")
            log(String(
                format: "render median %.1fms min %.1fms max %.1fms (n=10)",
                samples[5], samples[0], samples[9]
            ))
            log("png bytes \(png.count)")
            log("sha256 \(sha)")

            // The GPU path, on the same tile. Metal rather than OpenGL ES,
            // which iOS deprecated in 12; this is the code the SDK module
            // ships, not a copy of it.
            if let gpu = MetalTileRasterizer.createOrNull(tileSize: 512) {
                let tessellated = try renderer.tessellate(
                    z: 14, x: 14552, y: 6451, tileSize: 512, tiles: [tileData]
                )
                _ = gpu.renderPng(tessellated)  // warm the pipeline and textures

                var gpuSamples: [Double] = []
                var gpuPng = Data()
                for _ in 0..<10 {
                    let started = CFAbsoluteTimeGetCurrent()
                    let packed = try renderer.tessellate(
                        z: 14, x: 14552, y: 6451, tileSize: 512, tiles: [tileData]
                    )
                    guard let drawn = gpu.renderPng(packed) else { break }
                    gpuSamples.append((CFAbsoluteTimeGetCurrent() - started) * 1000)
                    gpuPng = drawn
                }
                gpuSamples.sort()

                if gpuSamples.count == 10 {
                    let timings = tessellated.timings
                    log(String(
                        format: "gpu median %.1fms min %.1fms max %.1fms (n=10)",
                        gpuSamples[5], gpuSamples[0], gpuSamples[9]
                    ))
                    log(String(
                        format: "gpu phases decode %.1fms tessellate %.1fms (filters %.1fms)",
                        timings.decode, timings.tessellate, timings.filterCompile
                    ))
                    log("gpu batches \(tessellated.batchCount) vertices "
                        + "\(tessellated.vertexFloatCount / TessellatedTile.vertexStride)")
                    log("gpu png bytes \(gpuPng.count)")

                    // Block averages, not raw pixels: MSAA and analytic
                    // coverage disagree on edges by construction, and the
                    // question worth asking is whether any region differs.
                    if let a = rgba(gpuPng), let b = rgba(png) {
                        let (percent, worst) = blockDifference(a, b, size: 512)
                        log(String(format: "gpu vs cpu blocks %.2f%% worst %d", percent, worst))
                    }
                    try gpuPng.write(
                        to: FileManager.default
                            .urls(for: .documentDirectory, in: .userDomainMask)[0]
                            .appendingPathComponent("tokyo-gpu.png")
                    )
                } else {
                    log("gpu FAILED after \(gpuSamples.count) tiles")
                }
            } else {
                log("gpu unavailable on this device")
            }

            // Restyle: geometry unchanged, so this is one re-rasterise.
            let restyleStarted = CFAbsoluteTimeGetCurrent()
            try renderer.setStyle(
                ##"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#ff0000"}}]}"##
            )
            let restyled = try renderer.render(
                z: 14, x: 14552, y: 6451, tileSize: 512, tiles: [tileData]
            )
            log(String(
                format: "restyle+render %.1fms (%d bytes)",
                (CFAbsoluteTimeGetCurrent() - restyleStarted) * 1000, restyled.count
            ))

            // Marker tiles, on this device rather than the simulator. The
            // simulator runs on the Mac's CPU, where both encoders are fast
            // enough that the comparison says little about an iPad.
            for line in markerTileReport() {
                log(line)
            }

            let documents = FileManager.default
                .urls(for: .documentDirectory, in: .userDomainMask)[0]
            try png.write(to: documents.appendingPathComponent("tokyo.png"))
            log("wrote Documents/tokyo.png")
            log("DONE")

            // The console stream never returns for a long-lived app, so the
            // report is also written where devicectl can copy it out.
            try lines.joined(separator: "\n")
                .write(
                    to: documents.appendingPathComponent("report.txt"),
                    atomically: true,
                    encoding: .utf8
                )

            DispatchQueue.main.async { image = UIImage(data: png) }
        } catch {
            log("FAILED: \(error)")
        }
    }
}

/// Decodes PNG bytes to straight-alpha RGBA over opaque white, so
/// premultiplication cannot skew a comparison.
private func rgba(_ png: Data) -> [UInt8]? {
    guard let image = UIImage(data: png)?.cgImage else { return nil }
    let size = image.width
    var buffer = [UInt8](repeating: 0, count: size * size * 4)
    guard let context = CGContext(
        data: &buffer,
        width: size, height: size,
        bitsPerComponent: 8, bytesPerRow: size * 4,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }
    context.setFillColor(red: 1, green: 1, blue: 1, alpha: 1)
    context.fill(CGRect(x: 0, y: 0, width: size, height: size))
    context.draw(image, in: CGRect(x: 0, y: 0, width: size, height: size))
    return buffer
}

/// Share of 8x8 blocks whose average colour differs by more than 24/255, and
/// the worst single channel delta.
private func blockDifference(_ a: [UInt8], _ b: [UInt8], size: Int) -> (Double, Int) {
    let block = 8
    let blocks = size / block
    var differing = 0
    var worst = 0
    for blockY in 0..<blocks {
        for blockX in 0..<blocks {
            var worstChannel = 0
            for channel in 0..<3 {
                var sumA = 0
                var sumB = 0
                for y in 0..<block {
                    let row = (blockY * block + y) * size * 4
                    for x in 0..<block {
                        let index = row + (blockX * block + x) * 4 + channel
                        sumA += Int(a[index])
                        sumB += Int(b[index])
                    }
                }
                let pixels = block * block
                worstChannel = max(worstChannel, abs(sumA / pixels - sumB / pixels))
            }
            if worstChannel > 24 { differing += 1 }
            worst = max(worst, worstChannel)
        }
    }
    return (Double(differing) * 100 / Double(blocks * blocks), worst)
}


/// Times the marker tile path and the two PNG encoders on real hardware.
private func markerTileReport() -> [String] {
    var lines: [String] = []

    func median(_ samples: [Double]) -> Double { samples.sorted()[samples.count / 2] }

    for markerCount in [2_000, 20_000] {
        let manager = MarkerManager<Int>.defaultManager()
        var seed: UInt64 = 12345
        func next() -> Double {
            seed = seed &* 6364136223846793005 &+ 1442695040888963407
            return Double(seed >> 11) / Double(1 << 53)
        }
        for _ in 0..<markerCount {
            let position = GeoPoint(latitude: 35.5 + next() * 0.4,
                                    longitude: 139.5 + next() * 0.5)
            manager.registerEntity(MarkerEntity<Int>(
                marker: nil, state: MarkerState(position: position),
                visible: true, isRendered: true, tiling: true
            ))
        }

        for (z, x, y) in [(12, 3638, 1612), (6, 56, 25)] {
            var png: Data?
            // A fresh renderer per round: its cache would answer every call
            // after the first, which is not what this measures.
            let samples = (0..<3).map { _ -> Double in
                let renderer = MarkerTileRenderer<Int>(markerManager: manager, tileSize: 512)
                let started = CFAbsoluteTimeGetCurrent()
                png = renderer.renderTile(request: TileRequest(x: x, y: y, z: z))
                return (CFAbsoluteTimeGetCurrent() - started) * 1000
            }
            guard let png, let image = UIImage(data: png), let cgImage = image.cgImage else {
                lines.append("markertile n=\(markerCount) z=\(z) EMPTY")
                continue
            }

            // Both encoders on the same pixels, so the comparison is not about
            // what the tile happened to contain.
            let width = cgImage.width
            let height = cgImage.height
            var pixels = [UInt8](repeating: 0, count: width * height * 4)
            pixels.withUnsafeMutableBytes { raw in
                CGContext(
                    data: raw.baseAddress, width: width, height: height,
                    bitsPerComponent: 8, bytesPerRow: width * 4,
                    space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
                )?.draw(cgImage, in: CGRect(x: 0, y: 0, width: width, height: height))
            }

            var platformBytes = 0
            let platform = median((0..<5).map { _ in
                let started = CFAbsoluteTimeGetCurrent()
                let data = image.pngData()
                let elapsed = (CFAbsoluteTimeGetCurrent() - started) * 1000
                platformBytes = data?.count ?? 0
                return elapsed
            })

            var rustBytes = 0
            let rust = median((0..<5).map { _ in
                var scratch = pixels
                var outPointer: UnsafeMutablePointer<UInt8>?
                var outLength = 0
                let started = CFAbsoluteTimeGetCurrent()
                _ = scratch.withUnsafeMutableBufferPointer { buffer in
                    tile_png_encode(buffer.baseAddress, UInt32(width), UInt32(height), 1,
                                    &outPointer, &outLength)
                }
                let elapsed = (CFAbsoluteTimeGetCurrent() - started) * 1000
                rustBytes = outLength
                if let outPointer { tile_png_buffer_free(outPointer, outLength) }
                return elapsed
            })

            lines.append(String(
                format: "markertile n=%d z=%d render=%.1fms tile=%dpx | png platform=%.1fms/%dB rust=%.1fms/%dB",
                markerCount, z, median(samples), width,
                platform, platformBytes, rust, rustBytes
            ))
        }
    }
    return lines
}
