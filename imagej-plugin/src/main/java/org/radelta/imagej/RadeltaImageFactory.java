package org.radelta.imagej;

import ij.CompositeImage;
import ij.ImageListener;
import ij.ImagePlus;

import java.io.File;
import java.io.IOException;
import java.util.Properties;

/** Shared ImagePlus construction used by both the menu command and ImageJ's
 * HandleExtraFileTypes drag/drop bridge. */
public final class RadeltaImageFactory {
    private RadeltaImageFactory() {}

    /**
     * Opens a Radelta file as an ImagePlus backed by a RadeltaVirtualStack.
     * The returned object is deliberately not wrapped as a CompositeImage;
     * ImageJ's Opener does that itself when this method is used through
     * HandleExtraFileTypes.
     */
    public static ImagePlus openBase(String path) throws IOException {
        final RadeltaFile rf = RadeltaFile.open(path);
        final RadeltaVirtualStack stack = new RadeltaVirtualStack(rf);

        final ImagePlus base = new ImagePlus(new File(path).getName(), stack);
        base.setDimensions(rf.channels, rf.slices, rf.frames);
        base.setOpenAsHyperStack(rf.channels > 1 || rf.frames > 1);
        try {
            RadeltaMetadata.apply(base, rf.readMetadata());
        } catch (IOException | RuntimeException e) {
            rf.close();
            throw e;
        }
        base.setProperty("Radelta.Format", formatName(rf.format));
        base.setProperty("Radelta.Lossy", Boolean.toString(rf.lossy));
        base.setProperty("Radelta.Streaming", Boolean.toString(rf.streaming));
        base.setProperty("Radelta.Source", path);

        // ImageJ's HandleExtraFileTypes path may wrap 'base' in a new
        // CompositeImage.  Close the native file whenever any ImagePlus that
        // owns this exact stack is closed, rather than testing object identity
        // against only the original base ImagePlus.
        final ImageListener listener = new ImageListener() {
            private boolean closed = false;

            @Override public void imageOpened(ImagePlus imp) {}
            @Override public void imageUpdated(ImagePlus imp) {}

            @Override public void imageClosed(ImagePlus imp) {
                if (!closed && imp != null && imp.getStack() == stack) {
                    closed = true;
                    stack.close();
                    ImagePlus.removeImageListener(this);
                }
            }
        };
        ImagePlus.addImageListener(listener);

        return base;
    }

    /** Opens a Radelta file for the plugin's own menu command. */
    public static ImagePlus openForDisplay(String path) throws IOException {
        ImagePlus base = openBase(path);
        if (base.getNChannels() > 1) {
            CompositeImage composite = new CompositeImage(base, CompositeImage.COMPOSITE);
            copyProperties(composite, base);
            return composite;
        }
        return base;
    }

    /** Copy the image payload/metadata needed by HandleExtraFileTypes into
     * the plugin instance ImagePlus that ImageJ will receive. */
    public static void adoptInto(ImagePlus target, ImagePlus source) {
        target.setStack(source.getTitle(), source.getStack());
        target.setDimensions(source.getNChannels(), source.getNSlices(), source.getNFrames());
        target.setOpenAsHyperStack(source.getOpenAsHyperStack());
        target.setCalibration(source.getCalibration());
        if (source.getOriginalFileInfo() != null) {
            target.setFileInfo(source.getOriginalFileInfo());
        }

        copyProperties(target, source);
    }

    private static void copyProperties(ImagePlus target, ImagePlus source) {
        String[] imageProperties = source.getPropertiesAsArray();
        if (imageProperties != null) target.setProperties(imageProperties);
        Properties properties = source.getProperties();
        if (properties != null) {
            for (Object key : properties.keySet()) {
                Object value = properties.get(key);
                if (key != null && value != null) {
                    target.setProperty(key.toString(), value);
                }
            }
        }
    }

    public static boolean isRadeltaPath(String path) {
        if (path == null) return false;
        String lower = path.toLowerCase(java.util.Locale.ROOT);
        return lower.endsWith(".rdlt") || lower.endsWith(".radelta");
    }

    public static String formatName(int f) {
        switch (f) {
            case 1: return "RDL1";
            case 2: return "RDLQ";
            case 3: return "RDM2";
            case 4: return "RDQ2";
            case 5: return "RDS3";
            case 6: return "RQS3";
            default: return "unknown(" + f + ")";
        }
    }
}
