package org.radelta.imagej;

import ij.ImagePlus;
import ij.measure.Calibration;

import org.w3c.dom.Element;
import org.w3c.dom.NodeList;

import java.io.*;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.*;

import javax.xml.XMLConstants;
import javax.xml.parsers.DocumentBuilderFactory;

/** Application-owned metadata. No Java object deserialization is used. */
final class RadeltaMetadata {
    static final String ORIGINAL = "Radelta.OriginalMetadata";
    private static final byte[] MAGIC = "RDIJ0001".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] TIFF = "RDTIFF01".getBytes(StandardCharsets.US_ASCII);

    static byte[] capture(ImagePlus imp) throws IOException {
        Map<String, String> values = new TreeMap<String, String>();
        values.put("title", imp.getTitle());
        Calibration c = imp.getCalibration();
        double[] numbers = {
            c.pixelWidth,
            c.pixelHeight,
            c.pixelDepth,
            c.frameInterval,
            c.fps,
            c.xOrigin,
            c.yOrigin,
            c.zOrigin
        };
        String[] keys = {
            "width", "height", "depth", "interval", "fps", "xOrigin", "yOrigin", "zOrigin"
        };
        for (int i = 0; i < keys.length; i++) values.put(keys[i], Double.toString(numbers[i]));
        values.put("xUnit", c.getXUnit());
        values.put("yUnit", c.getYUnit());
        values.put("zUnit", c.getZUnit());
        values.put("timeUnit", c.getTimeUnit());
        values.put("valueUnit", c.getValueUnit());
        values.put("function", Integer.toString(c.getFunction()));
        values.put("zeroClip", Boolean.toString(c.zeroClip()));
        values.put("invertY", Boolean.toString(c.getInvertY()));
        values.put("loop", Boolean.toString(c.loop));
        if (c.info != null) values.put("calibrationInfo", c.info);
        double[] coefficients = c.getCoefficients();
        if (coefficients != null)
            for (int i = 0; i < coefficients.length; i++)
                values.put("coefficient." + i, Double.toString(coefficients[i]));
        float[] table = c.getFunction() == Calibration.CUSTOM ? c.getCTable() : null;
        if (table != null) {
            ByteBuffer buffer = ByteBuffer.allocate(table.length * 4);
            for (float value : table) buffer.putFloat(value);
            values.put("calibrationTable", Base64.getEncoder().encodeToString(buffer.array()));
        }
        Properties properties = imp.getProperties();
        if (properties != null)
            for (Map.Entry<Object, Object> entry : properties.entrySet()) {
                if (!(entry.getKey() instanceof String)) continue;
                String key = (String) entry.getKey();
                if (key.startsWith("Radelta.")) continue;
                String value = encodeProperty(entry.getValue());
                if (value != null) values.put("property." + key, value);
            }
        String[] imageProperties = imp.getPropertiesAsArray();
        if (imageProperties != null)
            for (int i = 0; i + 1 < imageProperties.length; i += 2)
                values.put("imageProperty." + imageProperties[i], imageProperties[i + 1]);
        for (int i = 1; i <= imp.getStackSize(); i++) {
            String label = imp.getStack().getSliceLabel(i);
            if (label != null) values.put("label." + i, label);
        }
        byte[] original =
                imp.getProperty(ORIGINAL) instanceof byte[]
                        ? (byte[]) imp.getProperty(ORIGINAL)
                        : new byte[0];
        long limit = RadeltaNative.api().radelta_get_metadata_limit().longValue();
        long size = 8L + 4 + original.length + 4;
        for (Map.Entry<String, String> entry : values.entrySet()) {
            size +=
                    8L
                            + entry.getKey().getBytes(StandardCharsets.UTF_8).length
                            + entry.getValue().getBytes(StandardCharsets.UTF_8).length;
        }
        if (size > limit || size > Integer.MAX_VALUE)
            throw new IOException("ImageJ metadata exceeds the configured or Java array limit");
        ByteArrayOutputStream bytes = new ByteArrayOutputStream((int) size);
        DataOutputStream out = new DataOutputStream(bytes);
        out.write(MAGIC);
        writeBytes(out, original);
        out.writeInt(values.size());
        for (Map.Entry<String, String> entry : values.entrySet()) {
            writeBytes(out, entry.getKey().getBytes(StandardCharsets.UTF_8));
            writeBytes(out, entry.getValue().getBytes(StandardCharsets.UTF_8));
        }
        return bytes.toByteArray();
    }

    static void apply(ImagePlus imp, byte[] data) throws IOException {
        if (data.length == 0) return;
        byte[] original = data;
        Map<String, String> values = null;
        if (starts(data, MAGIC)) {
            DataInputStream in =
                    new DataInputStream(new ByteArrayInputStream(data, 8, data.length - 8));
            original = readBytes(in);
            int count = in.readInt();
            if (count < 0 || count > in.available() / 8)
                throw new IOException("Invalid ImageJ metadata field count");
            values = new HashMap<String, String>();
            for (int i = 0; i < count; i++) {
                String key = new String(readBytes(in), StandardCharsets.UTF_8);
                String value = new String(readBytes(in), StandardCharsets.UTF_8);
                if (values.put(key, value) != null)
                    throw new IOException("Duplicate ImageJ metadata field");
            }
            if (in.available() != 0) throw new IOException("Trailing ImageJ metadata bytes");
        }
        imp.setProperty(ORIGINAL, original);
        if (values == null && starts(original, TIFF)) applyTiff(imp, original);
        if (values != null) applySnapshot(imp, values);
    }

    private static void applySnapshot(ImagePlus imp, Map<String, String> p) throws IOException {
        try {
            Calibration c = new Calibration(imp);
            c.pixelWidth = number(p, "width");
            c.pixelHeight = number(p, "height");
            c.pixelDepth = number(p, "depth");
            c.frameInterval = number(p, "interval");
            c.fps = number(p, "fps");
            c.xOrigin = number(p, "xOrigin");
            c.yOrigin = number(p, "yOrigin");
            c.zOrigin = number(p, "zOrigin");
            c.setXUnit(p.get("xUnit"));
            c.setYUnit(p.get("yUnit"));
            c.setZUnit(p.get("zUnit"));
            c.setTimeUnit(p.get("timeUnit"));
            c.info = p.get("calibrationInfo");
            c.setInvertY(Boolean.parseBoolean(p.get("invertY")));
            c.loop = Boolean.parseBoolean(p.get("loop"));
            int count = 0;
            while (p.containsKey("coefficient." + count)) count++;
            double[] coefficients = count == 0 ? null : new double[count];
            for (int i = 0; i < count; i++) coefficients[i] = number(p, "coefficient." + i);
            int function = Integer.parseInt(p.get("function"));
            if (function == Calibration.CUSTOM && p.containsKey("calibrationTable")) {
                ByteBuffer buffer =
                        ByteBuffer.wrap(Base64.getDecoder().decode(p.get("calibrationTable")));
                if (buffer.remaining() % 4 != 0)
                    throw new IOException("Invalid calibration table length");
                float[] table = new float[buffer.remaining() / 4];
                for (int i = 0; i < table.length; i++) table[i] = buffer.getFloat();
                c.setCTable(table, p.get("valueUnit"));
            } else
                c.setFunction(
                        function,
                        coefficients,
                        p.get("valueUnit"),
                        Boolean.parseBoolean(p.get("zeroClip")));
            imp.setCalibration(c);
            if (p.containsKey("title")) imp.setTitle(p.get("title"));
            for (Map.Entry<String, String> entry : p.entrySet())
                if (entry.getKey().startsWith("property.")) {
                    String key = entry.getKey().substring(9);
                    if (!key.startsWith("Radelta."))
                        imp.setProperty(key, decodeProperty(entry.getValue()));
                }
            for (Map.Entry<String, String> entry : p.entrySet())
                if (entry.getKey().startsWith("imageProperty."))
                    imp.setProp(entry.getKey().substring(14), entry.getValue());
            String[] labels = new String[imp.getStackSize()];
            for (int i = 0; i < labels.length; i++) labels[i] = p.get("label." + (i + 1));
            if (imp.getStack() instanceof RadeltaVirtualStack)
                ((RadeltaVirtualStack) imp.getStack()).setMetadataLabels(labels);
            else
                for (int i = 0; i < labels.length; i++)
                    imp.getStack().setSliceLabel(labels[i], i + 1);
        } catch (IllegalArgumentException | NullPointerException e) {
            throw new IOException("Invalid ImageJ metadata snapshot", e);
        }
    }

    private static double number(Map<String, String> p, String key) {
        return Double.parseDouble(p.get(key));
    }

    private static String encodeProperty(Object value) {
        if (value instanceof String) return "T" + value;
        if (value instanceof Integer) return "I" + value;
        if (value instanceof Long) return "L" + value;
        if (value instanceof Double) return "D" + value;
        if (value instanceof Float) return "F" + value;
        if (value instanceof Short) return "S" + value;
        if (value instanceof Byte) return "B" + value;
        if (value instanceof Boolean) return "Z" + value;
        if (value instanceof byte[])
            return "Y" + Base64.getEncoder().encodeToString((byte[]) value);
        return null;
    }

    private static Object decodeProperty(String value) throws IOException {
        if (value.isEmpty()) throw new IOException("Invalid ImageJ property");
        String text = value.substring(1);
        switch (value.charAt(0)) {
            case 'T':
                return text;
            case 'I':
                return Integer.valueOf(text);
            case 'L':
                return Long.valueOf(text);
            case 'D':
                return Double.valueOf(text);
            case 'F':
                return Float.valueOf(text);
            case 'S':
                return Short.valueOf(text);
            case 'B':
                return Byte.valueOf(text);
            case 'Z':
                return Boolean.valueOf(text);
            case 'Y':
                return Base64.getDecoder().decode(text);
            default:
                throw new IOException("Unsupported ImageJ property type");
        }
    }

    private static void writeBytes(DataOutputStream out, byte[] bytes) throws IOException {
        out.writeInt(bytes.length);
        out.write(bytes);
    }

    private static byte[] readBytes(DataInputStream in) throws IOException {
        int length = in.readInt();
        if (length < 0 || length > in.available())
            throw new IOException("Truncated ImageJ metadata");
        byte[] bytes = new byte[length];
        in.readFully(bytes);
        return bytes;
    }

    private static boolean starts(byte[] data, byte[] signature) {
        if (data.length < signature.length) return false;
        for (int i = 0; i < signature.length; i++) if (data[i] != signature[i]) return false;
        return true;
    }

    // Read the TIFF adapter's typed records without interpreting unknown tags.
    // The original payload remains attached unchanged, including nested records.
    private static void applyTiff(ImagePlus imp, byte[] data) throws IOException {
        try {
            ByteBuffer in = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
            in.position(8);
            boolean valid = in.get() != 0;
            long pages = in.getLong();
            if (pages != imp.getStackSize() || pages > in.remaining() / 8)
                throw new IOException("TIFF metadata plane count mismatch");
            in.position(in.position() + (int) pages * 8);
            long ifds = in.getLong();
            if (ifds != 1 && ifds != pages)
                throw new IOException("TIFF metadata IFD count mismatch");
            Map<Integer, byte[]> tags = new HashMap<Integer, byte[]>();
            for (long i = 0; i < ifds; i++) readTags(in, i == 0 ? tags : null, 0);
            if (in.hasRemaining()) throw new IOException("Trailing TIFF metadata bytes");
            Calibration c = imp.getCalibration();
            int unit =
                    tags.containsKey(296) && tags.get(296).length >= 2
                            ? ByteBuffer.wrap(tags.get(296))
                                    .order(ByteOrder.LITTLE_ENDIAN)
                                    .getShort()
                            : 1;
            double factor = unit == 2 ? 25400 : unit == 3 ? 10000 : 1;
            if (unit == 2 || unit == 3) c.setUnit("um");
            c.pixelWidth = factor / rational(tags.get(282));
            c.pixelHeight = factor / rational(tags.get(283));
            if (tags.containsKey(270)) {
                byte[] bytes = tags.get(270);
                int end = bytes.length;
                while (end > 0 && bytes[end - 1] == 0) end--;
                String description = new String(bytes, 0, end, StandardCharsets.UTF_8);
                imp.setProperty("Info", description);
                if (valid && description.startsWith("ImageJ=")) {
                    Properties p = new Properties();
                    p.load(new StringReader(description));
                    if (p.containsKey("unit")) c.setUnit(p.getProperty("unit"));
                    c.pixelDepth = optionalNumber(p, "spacing", c.pixelDepth);
                    c.frameInterval = optionalNumber(p, "finterval", c.frameInterval);
                    c.fps = optionalNumber(p, "fps", c.fps);
                    c.xOrigin = optionalNumber(p, "xorigin", c.xOrigin);
                    c.yOrigin = optionalNumber(p, "yorigin", c.yOrigin);
                    c.zOrigin = optionalNumber(p, "zorigin", c.zOrigin);
                    if (p.containsKey("tunit")) c.setTimeUnit(p.getProperty("tunit"));
                } else if (valid && description.contains("<OME")) applyOme(c, description);
            }
        } catch (java.nio.BufferUnderflowException | IllegalArgumentException e) {
            throw new IOException("Invalid TIFF metadata", e);
        }
    }

    private static void readTags(ByteBuffer in, Map<Integer, byte[]> tags, int depth)
            throws IOException {
        if (depth > 16) throw new IOException("TIFF metadata nesting exceeds 16");
        long count = in.getLong();
        if (count < 0 || count > in.remaining() / 20)
            throw new IOException("Invalid TIFF metadata tag count");
        for (long i = 0; i < count; i++) {
            int tag = Short.toUnsignedInt(in.getShort());
            in.getShort();
            long length = in.getLong();
            if (length < 0 || length > in.remaining())
                throw new IOException("Invalid TIFF metadata value length");
            if (tags != null && (tag == 270 || tag == 282 || tag == 283 || tag == 296)) {
                byte[] value = new byte[(int) length];
                in.get(value);
                tags.put(tag, value);
            } else in.position(in.position() + (int) length);
            long children = in.getLong();
            if (children < 0 || children > in.remaining() / 8)
                throw new IOException("Invalid TIFF child count");
            for (long child = 0; child < children; child++) readTags(in, null, depth + 1);
        }
    }

    private static double rational(byte[] value) {
        if (value == null || value.length != 8) return 1;
        ByteBuffer in = ByteBuffer.wrap(value).order(ByteOrder.LITTLE_ENDIAN);
        long n = Integer.toUnsignedLong(in.getInt()), d = Integer.toUnsignedLong(in.getInt());
        return n == 0 || d == 0 ? 1 : (double) n / d;
    }

    private static double optionalNumber(Properties p, String key, double fallback) {
        return p.containsKey(key) ? Double.parseDouble(p.getProperty(key)) : fallback;
    }

    private static void applyOme(Calibration c, String xml) throws IOException {
        try {
            DocumentBuilderFactory factory = DocumentBuilderFactory.newInstance();
            factory.setNamespaceAware(true);
            factory.setFeature("http://apache.org/xml/features/disallow-doctype-decl", true);
            factory.setFeature("http://xml.org/sax/features/external-general-entities", false);
            factory.setFeature("http://xml.org/sax/features/external-parameter-entities", false);
            factory.setAttribute(XMLConstants.ACCESS_EXTERNAL_DTD, "");
            factory.setAttribute(XMLConstants.ACCESS_EXTERNAL_SCHEMA, "");
            NodeList list =
                    factory.newDocumentBuilder()
                            .parse(new ByteArrayInputStream(xml.getBytes(StandardCharsets.UTF_8)))
                            .getElementsByTagNameNS("*", "Pixels");
            if (list.getLength() == 0) return;
            Element pixels = (Element) list.item(0);
            String[] names = {"PhysicalSizeX", "PhysicalSizeY", "PhysicalSizeZ", "TimeIncrement"};
            for (int i = 0; i < names.length; i++)
                if (pixels.hasAttribute(names[i])) {
                    double value = Double.parseDouble(pixels.getAttribute(names[i]));
                    String unit =
                            pixels.hasAttribute(names[i] + "Unit")
                                    ? pixels.getAttribute(names[i] + "Unit")
                                    : (i == 3 ? "sec" : "um");
                    if (i == 0) {
                        c.pixelWidth = value;
                        c.setXUnit(unit);
                    } else if (i == 1) {
                        c.pixelHeight = value;
                        c.setYUnit(unit);
                    } else if (i == 2) {
                        c.pixelDepth = value;
                        c.setZUnit(unit);
                    } else {
                        c.frameInterval = value;
                        c.setTimeUnit(unit);
                    }
                }
        } catch (Exception e) {
            throw new IOException("Invalid OME calibration metadata", e);
        }
    }

    private RadeltaMetadata() {}
}
