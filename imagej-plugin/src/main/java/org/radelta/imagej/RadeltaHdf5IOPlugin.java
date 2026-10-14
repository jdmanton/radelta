package org.radelta.imagej;

import ij.IJ;
import ij.ImagePlus;

import org.scijava.Priority;
import org.scijava.io.AbstractIOPlugin;
import org.scijava.io.IOPlugin;
import org.scijava.io.location.FileLocation;
import org.scijava.io.location.Location;
import org.scijava.plugin.Attr;
import org.scijava.plugin.Plugin;

import java.io.IOException;

/**
 * Eager SciJava I/O hook for HDF5 files containing Radelta datasets.
 *
 * <p>Fiji has no ImageJ1 handler for plain {@code .h5} files, so File &gt; Open
 * and drag-and-drop otherwise end at Bio-Formats, which cannot decode them.
 * Like {@link RadeltaIOPlugin}, this runs before that fallback. It claims only
 * files with a Radelta-filtered dataset, leaving other HDF5 files (for example
 * BigDataViewer or ilastik data) to their own readers.
 */
@Plugin(
        type = IOPlugin.class,
        name = "Radelta HDF5 I/O",
        priority = Priority.VERY_HIGH,
        attrs = { @Attr(name = "eager") }
)
public class RadeltaHdf5IOPlugin extends AbstractIOPlugin<Object> {

    @Override
    public Class<Object> getDataType() {
        return Object.class;
    }

    @Override
    public boolean supportsOpen(String source) {
        return RadeltaHdf5Opener.supports(source);
    }

    /**
     * With Fiji's "SciJava I/O" opening option, the legacy bridge resolves
     * paths to locations; accept local files there as well.
     */
    @Override
    public boolean supportsOpen(Location source) {
        return source instanceof FileLocation && supportsOpen(((FileLocation) source).getFile().getPath());
    }

    @Override
    public Object open(Location source) throws IOException {
        return source instanceof FileLocation ? open(((FileLocation) source).getFile().getPath()) : null;
    }

    @Override
    public Object open(String source) throws IOException {
        if (!supportsOpen(source)) return null;
        try {
            ImagePlus imp = RadeltaHdf5Opener.open(source);
            if (imp != null) imp.show();
        } catch (IOException e) {
            // Report here: a thrown exception would also hand the file on to
            // Bio-Formats, which would then show a second, misleading error.
            IJ.error("Radelta HDF5", e.getMessage());
        }
        // As for .rdlt files, any non-null result tells the legacy opener the
        // request was handled, including cancellation.
        return Boolean.TRUE;
    }
}
