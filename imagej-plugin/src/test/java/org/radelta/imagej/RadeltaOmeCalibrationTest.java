package org.radelta.imagej;

import static org.junit.Assert.*;

import ij.ImagePlus;
import ij.ImageStack;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import javax.xml.XMLConstants;
import javax.xml.parsers.DocumentBuilderFactory;
import org.junit.Test;

public class RadeltaOmeCalibrationTest {
    private static final String PROPERTY = "javax.xml.parsers.DocumentBuilderFactory";
    private static final String[] PROVIDERS = {
        "com.sun.org.apache.xerces.internal.jaxp.DocumentBuilderFactoryImpl",
        "org.apache.xerces.jaxp.DocumentBuilderFactoryImpl"
    };

    private static String ome(boolean prefix) {
        String xml = "<OME xmlns=\"http://www.openmicroscopy.org/Schemas/OME/2016-06\">"
                + "<Image><Pixels SizeT=\"1\" SizeC=\"4\" SizeZ=\"4\""
                + " PhysicalSizeX=\"0.125\" PhysicalSizeY=\"0.25\" PhysicalSizeZ=\"3.25\""
                + " TimeIncrement=\"4.5\" TimeIncrementUnit=\"ms\"/></Image></OME>";
        return prefix ? xml.replace("xmlns=", "xmlns:o=")
                .replace("<OME", "<o:OME").replace("</OME", "</o:OME")
                .replace("<Image", "<o:Image").replace("</Image", "</o:Image")
                .replace("<Pixels", "<o:Pixels") : xml;
    }

    private static ImagePlus apply(String xml) throws IOException {
        // A local four-plane channel retains the acquisition-wide OME sizes.
        ImageStack stack = new ImageStack(3, 2);
        for (int p = 0; p < 4; p++) stack.addSlice("", new short[6]);
        ImagePlus image = new ImagePlus("local channel", stack);
        image.setDimensions(1, 4, 1);
        byte[] desc = (xml + "\0").getBytes(StandardCharsets.UTF_8);
        ByteBuffer out = ByteBuffer.allocate(8 + 1 + 8 + 4 * 8 + 8 + 8 + 20 + desc.length)
                .order(ByteOrder.LITTLE_ENDIAN);
        out.put("RDTIFF01".getBytes(StandardCharsets.US_ASCII)).put((byte) 1).putLong(4);
        for (int p = 0; p < 4; p++) out.putLong(p);
        out.putLong(1).putLong(1).putShort((short) 270).putShort((short) 2);
        out.putLong(desc.length).put(desc).putLong(0);
        byte[] metadata = out.array();
        RadeltaMetadata.apply(image, metadata);
        assertEquals(xml, image.getProperty("Info"));
        assertArrayEquals(metadata, (byte[]) image.getProperty(RadeltaMetadata.ORIGINAL));
        assertEquals(1, image.getNChannels());
        assertEquals(4, image.getNSlices());
        assertEquals(1, image.getNFrames());
        return image;
    }

    private interface Check { void run() throws Exception; }

    private static void withProvider(String provider, Check check) throws Exception {
        String previous = System.getProperty(PROPERTY);
        try {
            System.setProperty(PROPERTY, provider);
            assertEquals(provider, DocumentBuilderFactory.newInstance().getClass().getName());
            check.run();
        } finally {
            if (previous == null) System.clearProperty(PROPERTY);
            else System.setProperty(PROPERTY, previous);
        }
    }

    @Test
    public void fijiXercesReproducesUnsupportedJaxpAttributes() throws Exception {
        withProvider(PROVIDERS[1], () -> {
            for (String property : new String[] {
                    XMLConstants.ACCESS_EXTERNAL_DTD, XMLConstants.ACCESS_EXTERNAL_SCHEMA}) {
                try {
                    DocumentBuilderFactory.newInstance().setAttribute(property, "");
                    fail("Expected Fiji Xerces to reject " + property);
                } catch (IllegalArgumentException expected) {
                    assertTrue(expected.getMessage().contains(property));
                }
            }
        });
    }

    @Test
    public void calibrationLoadsWithJdkAndFijiParsers() throws Exception {
        for (String provider : PROVIDERS) withProvider(provider, () -> {
            for (boolean prefix : new boolean[] {false, true}) {
                ImagePlus image = apply(ome(prefix));
                assertEquals(.125, image.getCalibration().pixelWidth, 0);
                assertEquals(.25, image.getCalibration().pixelHeight, 0);
                assertEquals(3.25, image.getCalibration().pixelDepth, 0);
                assertEquals(4.5, image.getCalibration().frameInterval, 0);
                assertEquals("ms", image.getCalibration().getTimeUnit());
            }
        });
    }

    @Test
    public void doctypesAndExternalEntitiesRemainRejected() throws Exception {
        for (String provider : PROVIDERS) withProvider(provider, () -> {
            for (String declaration : new String[] {
                    "<!DOCTYPE OME SYSTEM \"http://127.0.0.1:1/no-network.dtd\">",
                    "<!DOCTYPE OME [<!ENTITY external SYSTEM \"file:///radelta-no-read\">]>",
                    "<!DOCTYPE OME [<!ENTITY local \"3.25\">]>"}) {
                try {
                    apply(declaration + ome(false));
                    fail("Accepted a DOCTYPE with " + provider);
                } catch (IOException expected) {
                    assertNotNull(expected.getCause());
                    assertTrue(expected.getCause().getMessage().contains("DOCTYPE"));
                }
            }
        });
    }

    @Test
    public void xincludeDoesNotLoadExternalCalibration() throws Exception {
        Path external = Files.createTempFile("radelta-xml-include-", ".xml");
        try {
            Files.write(external, "<Pixels PhysicalSizeX=\"99\"/>".getBytes(StandardCharsets.UTF_8));
            String xml = ome(false).replace("<Image>",
                    "<xi:include xmlns:xi=\"http://www.w3.org/2001/XInclude\" href=\""
                    + external.toUri().toASCIIString() + "\"/><Image>");
            for (String provider : PROVIDERS) withProvider(provider, () -> {
                assertEquals(.125, apply(xml).getCalibration().pixelWidth, 0);
            });
        } finally {
            Files.deleteIfExists(external);
        }
    }

    @Test
    public void malformedXmlAndInvalidCalibrationStillFail() throws Exception {
        for (String provider : PROVIDERS) withProvider(provider, () -> {
            for (String xml : new String[] {
                    ome(false).replace("</Image>", ""),
                    ome(false).replace("3.25", "invalid")}) {
                try {
                    apply(xml);
                    fail("Accepted invalid calibration with " + provider);
                } catch (IOException expected) {
                    assertEquals("Invalid OME calibration metadata", expected.getMessage());
                }
            }
        });
    }
}
