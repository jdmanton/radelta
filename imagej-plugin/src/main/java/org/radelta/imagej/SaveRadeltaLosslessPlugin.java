package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;
import ij.plugin.PlugIn;

public class SaveRadeltaLosslessPlugin implements PlugIn {
    @Override
    public void run(String arg) {
        try {
            ImagePlus imp = IJ.getImage();
            RadeltaExporter.saveLosslessInteractive(imp);
        } catch (Throwable e) {
            IJ.handleException(e);
        }
    }
}
