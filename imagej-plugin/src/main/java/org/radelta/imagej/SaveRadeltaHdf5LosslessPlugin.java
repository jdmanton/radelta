package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;
import ij.plugin.PlugIn;

public class SaveRadeltaHdf5LosslessPlugin implements PlugIn {
    @Override
    public void run(String arg) {
        try {
            ImagePlus imp = IJ.getImage();
            RadeltaHdf5Writer.saveInteractive(imp, false);
        } catch (Throwable e) {
            IJ.handleException(e);
        }
    }
}
