// The extensions the host GL provides, in the form an IRIX program expects to
// read from glGetString(GL_EXTENSIONS). An extension is here only when its
// entry points work: the ones the Mac has under the same name, the ones whose
// calls are the same as a core or ARB call it has, and the ones emulated in
// emul.rs. (Checked against SGI's own libGL list and the Mac's; carried over
// as generated, not regenerated here.)
pub const EXTENSIONS: &str = "GL_ARB_multitexture GL_EXT_abgr GL_EXT_blend_color GL_EXT_blend_logic_op GL_EXT_blend_minmax GL_EXT_blend_subtract GL_EXT_convolution GL_EXT_copy_texture GL_EXT_histogram GL_EXT_packed_pixels GL_EXT_polygon_offset GL_EXT_subtexture GL_EXT_texture GL_EXT_texture3D GL_EXT_texture_object GL_EXT_vertex_array GL_SGIS_detail_texture GL_SGIS_fog_function GL_SGIS_generate_mipmap GL_SGIS_multisample GL_SGIS_multitexture GL_SGIS_point_parameters GL_SGIS_sharpen_texture GL_SGIS_texture4D GL_SGIS_texture_edge_clamp GL_SGIS_texture_filter4 GL_SGIS_texture_lod GL_SGIS_texture_select GL_SGIX_clipmap GL_SGIX_flush_raster GL_SGIX_fog_offset GL_SGIX_interlace GL_SGIX_pixel_texture GL_SGIX_reference_plane GL_SGIX_shadow GL_SGIX_sprite GL_SGIX_texture_add_env GL_SGIX_texture_scale_bias GL_SGI_color_matrix GL_SGI_color_table GL_SGI_texture_color_table";

/*
 * What a guest program is told it is talking to.
 *
 * The host's own strings are the Mac's -- "Apple", "Apple M4 Pro", "2.1 Metal
 * - 91.7" -- and handing those to an IRIX program is wrong twice over: a
 * program that switches on the renderer string would be deciding what to do
 * from the wrong machine entirely, and anything that logs them records a
 * machine the user is not using. The same argument that makes the extension
 * list ours to state makes these ours to state.
 *
 * They say what is true: an OpenGL of IRIS's, at the version this library
 * implements (IRIX's gl.h is OpenGL 1.1, and that is the API a guest compiles
 * against). They do not claim to be SGI hardware -- a program looking for a
 * RealityEngine should not find one -- and they do not pretend to be the Mac.
 * The host's real strings go to the log at debug level, and setting
 * IRIS_HOSTGL_STRINGS=host in the *emulator's* environment passes them
 * straight through for when the host GL itself is what is being debugged.
 */
pub const VENDOR: &str = "IRIS";
pub const RENDERER: &str = "IRIS host OpenGL";
pub const VERSION: &str = "1.1 IRIS";

/// The GLX extensions answered, for glXQueryExtensionsString.
pub const GLX_EXTENSIONS: &str = "GLX_EXT_visual_info GLX_EXT_visual_rating GLX_SGIS_multisample GLX_SGIX_fbconfig GLX_SGIX_pbuffer GLX_SGIX_swap_group GLX_SGI_make_current_read GLX_SGI_swap_control GLX_SGI_video_sync";
