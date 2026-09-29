package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;
import ij.plugin.PlugIn;

public class SaveRadeltaLossyPlugin implements PlugIn {
    @Override
    public void run(String arg) {
        try {
            ImagePlus imp = IJ.getImage();
            RadeltaExporter.saveLossyInteractive(imp);
        } catch (Throwable e) {
            IJ.handleException(e);
        }
    }
}
