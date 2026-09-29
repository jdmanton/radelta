package org.radelta.imagej;

import ij.ImagePlus;

import org.scijava.Priority;
import org.scijava.io.AbstractIOPlugin;
import org.scijava.io.IOPlugin;
import org.scijava.plugin.Attr;
import org.scijava.plugin.Plugin;

import java.io.IOException;

/**
 * Eager SciJava I/O hook for Radelta files.
 *
 * Fiji's ImageJ2 legacy bridge gives eager IOPlugin implementations first
 * chance at File>Open and drag-and-drop requests, before falling back to
 * ImageJ1 HandleExtraFileTypes.  This is therefore the correct independent
 * extension point for .rdlt files and avoids replacing Fiji's global
 * HandleExtraFileTypes plugin.
 */
@Plugin(
        type = IOPlugin.class,
        name = "Radelta I/O",
        priority = Priority.VERY_HIGH,
        attrs = { @Attr(name = "eager") }
)
public class RadeltaIOPlugin extends AbstractIOPlugin<Object> {

    @Override
    public Class<Object> getDataType() {
        return Object.class;
    }

    @Override
    public boolean supportsOpen(String source) {
        return RadeltaImageFactory.isRadeltaPath(source);
    }

    @Override
    public Object open(String source) throws IOException {
        if (!supportsOpen(source)) return null;

        final ImagePlus imp = RadeltaImageFactory.openForDisplay(source);
        imp.show();

        // DefaultLegacyOpener interprets any non-null result as handled.  The
        // ImagePlus has already been shown deliberately, because its generic
        // handler only auto-displays ImageJ2 Dataset objects.
        return Boolean.TRUE;
    }
}
