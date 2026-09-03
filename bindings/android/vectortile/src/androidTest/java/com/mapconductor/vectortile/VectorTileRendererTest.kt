package com.mapconductor.vectortile

import android.graphics.BitmapFactory
import java.security.MessageDigest
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONArray
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Runs the Rust renderer on a real device against the MapLibre demo style and a
 * real vector tile, both bundled as assets so the test needs no network.
 */
@RunWith(AndroidJUnit4::class)
class VectorTileRendererTest {

    private lateinit var styleJson: String
    private lateinit var tileBytes: ByteArray
    private var renderer: VectorTileRenderer? = null

    @Before
    fun setUp() {
        val assets = InstrumentationRegistry.getInstrumentation().context.assets
        styleJson = assets.open("demo-style.json").use { it.readBytes().decodeToString() }
        tileBytes = assets.open("tile-0-0-0.pbf").use { it.readBytes() }
        renderer = VectorTileRenderer.create(styleJson)
    }

    @After
    fun tearDown() {
        renderer?.close()
    }

    @Test
    fun plansTheSourceTilesForATargetTile() {
        val plan = JSONArray(renderer!!.plan(0, 0, 0))
        assertEquals(1, plan.length())
        val entry = plan.getJSONObject(0)
        assertEquals("maplibre", entry.getString("sourceId"))
        assertTrue(entry.getString("url").endsWith("/0/0/0.pbf"))
        assertEquals(1.0, entry.getDouble("scale"), 0.001)
    }

    @Test
    fun overzoomsPastTheSourceMaxzoom() {
        // The demo source stops at z6, so z8 must fall back to its ancestor.
        val entry = JSONArray(renderer!!.plan(8, 142, 99)).getJSONObject(0)
        assertEquals(6, entry.getInt("z"))
        assertEquals(35, entry.getInt("x"))
        assertEquals(24, entry.getInt("y"))
        assertEquals(4.0, entry.getDouble("scale"), 0.001)
    }

    @Test
    fun rendersARealTileToADecodablePng() {
        val started = System.nanoTime()
        val png = renderer!!.render(0, 0, 0, 512, listOf(tileBytes))
        val elapsedMs = (System.nanoTime() - started) / 1_000_000.0

        assertTrue("expected a non-trivial PNG, got ${png.size} bytes", png.size > 10_000)

        val bitmap = BitmapFactory.decodeByteArray(png, 0, png.size)
        assertNotNull("PNG did not decode", bitmap)
        assertEquals(512, bitmap.width)
        assertEquals(512, bitmap.height)

        // The demo style paints an opaque ocean background everywhere, then
        // countries over it. A blank tile would fail both checks.
        val distinct = HashSet<Int>()
        var opaque = 0
        for (y in 0 until 512 step 4) {
            for (x in 0 until 512 step 4) {
                val pixel = bitmap.getPixel(x, y)
                if (pixel ushr 24 != 0) opaque++
                distinct.add(pixel)
            }
        }
        assertEquals(128 * 128, opaque)
        assertTrue("expected many colours, got ${distinct.size}", distinct.size > 10)

        // The whole point of a shared Rust core is that every platform draws
        // the same pixels. Hashing the PNG lets the three platforms be compared
        // exactly rather than by eye.
        val sha = MessageDigest.getInstance("SHA-256").digest(png)
            .joinToString("") { "%02x".format(it) }
        println("ANDROID_RENDER_MS=$elapsedMs bytes=${png.size}")
        println("ANDROID_PNG_SHA256=$sha")
    }

    @Test
    fun warmRenderTimingAcrossTiles() {
        val subject = renderer!!
        // Same source tile for every target: this measures decode plus
        // rasterise, which is the number that decides on-device viability.
        val samples = DoubleArray(10)
        for (i in samples.indices) {
            val started = System.nanoTime()
            subject.render(0, 0, 0, 512, listOf(tileBytes))
            samples[i] = (System.nanoTime() - started) / 1_000_000.0
        }
        samples.sort()
        val median = samples[samples.size / 2]
        println("ANDROID_WARM_MEDIAN_MS=$median min=${samples.first()} max=${samples.last()}")
        assertTrue("render is implausibly slow: $median ms", median < 5_000)
    }

    @Test
    fun restylingChangesPixelsWithoutNewTileData() {
        val subject = renderer!!
        val beforePng = subject.render(0, 0, 0, 256, listOf(tileBytes))
        val before = BitmapFactory.decodeByteArray(beforePng, 0, beforePng.size)
        assertNotNull(before)

        val flat = """
            {"version":8,"sources":{},"layers":[
              {"id":"bg","type":"background","paint":{"background-color":"#ff0000"}}
            ]}
        """.trimIndent()
        subject.setStyle(flat)

        // Same bytes in, different pixels out.
        val png = subject.render(0, 0, 0, 256, listOf(tileBytes))
        val after = BitmapFactory.decodeByteArray(png, 0, png.size)
        val pixel = after.getPixel(128, 128)
        assertEquals(0xFF, pixel ushr 24 and 0xFF)
        assertEquals(0xFF, pixel shr 16 and 0xFF)
        assertEquals(0x00, pixel shr 8 and 0xFF)
    }

    @Test
    fun reportsUndrawnLayerTypes() {
        val kinds = JSONArray(renderer!!.unsupportedLayerTypes())
        val values = (0 until kinds.length()).map { kinds.getString(it) }
        assertTrue("expected symbol to be reported, got $values", values.contains("symbol"))
    }

    @Test
    fun aBrokenStyleThrowsInsteadOfCrashingTheProcess() {
        try {
            VectorTileRenderer.create("{ not json")
            throw AssertionError("expected an exception")
        } catch (expected: IllegalArgumentException) {
            assertNotNull(expected.message)
        }
    }

    @Test
    fun closingTwiceIsSafe() {
        val subject = VectorTileRenderer.create(styleJson)
        subject.close()
        subject.close()
    }
}
