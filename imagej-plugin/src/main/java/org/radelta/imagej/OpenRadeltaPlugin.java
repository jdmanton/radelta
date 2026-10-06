package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;
import ij.io.OpenDialog;
import ij.plugin.PlugIn;

public class OpenRadeltaPlugin implements PlugIn {
    @Override
    public void run(String arg) {
        String path = arg;
        if (path == null || path.trim().isEmpty()) {
            OpenDialog od = new OpenDialog("Open Radelta...", null);
            path = od.getPath();
            if (path == null) return;
        }

        try {
            ImagePlus shown = RadeltaImageFactory.openForDisplay(path);
            if (shown == null) return;
            shown.show();
            IJ.showStatus("Radelta: " + shown.getWidth() + "x" + shown.getHeight() +
                    ", C=" + shown.getNChannels() + ", Z=" + shown.getNSlices() +
                    ", T=" + shown.getNFrames() +
                    ("true".equals(shown.getProperty("Radelta.Lossy")) ? " (lossy)" : " (lossless)") +
                    ", " + shown.getProperty("Radelta.OpenMode"));
        } catch (Throwable e) {
            IJ.handleException(e);
        }
    }
}
