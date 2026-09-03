import CryptoKit
import MapConductorVectorTile
import SwiftUI

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
