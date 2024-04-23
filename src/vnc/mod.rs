/*
 *   This file is part of NCC Group Scrying https://github.com/nccgroup/scrying
 *   Copyright 2020-2021 David Young <david(dot)young(at)nccgroup(dot)com>
 *   Released as open source by NCC Group Plc - https://www.nccgroup.com
 *
 *   Scrying is free software: you can redistribute it and/or modify
 *   it under the terms of the GNU General Public License as published by
 *   the Free Software Foundation, either version 3 of the License, or
 *   (at your option) any later version.
 *
 *   Scrying is distributed in the hope that it will be useful,
 *   but WITHOUT ANY WARRANTY; without even the implied warranty of
 *   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 *   GNU General Public License for more details.
 *
 *   You should have received a copy of the GNU General Public License
 *   along with Scrying.  If not, see <https://www.gnu.org/licenses/>.
*/

use crate::argparse::Mode::Vnc;
use crate::argparse::Opts;
use crate::parsing::Target;
use crate::reporting::ReportMessageContent;
use crate::reporting::{FileError, ReportMessage};
use crate::util::target_to_filename;
use crate::ThreadStatus;
#[allow(unused)]
use crate::{debug, error, info, trace, warn};
use color_eyre::{eyre::eyre, Result};
use image::{DynamicImage, ImageBuffer, Rgb};
use std::cmp::min;
use std::convert::TryInto;
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;
use vnc::client::{AuthChoice, AuthMethod, Client};
use vnc::Colour;
use vnc::{PixelFormat, Rect};

//TODO code reuse with RDP?
struct Image {
    image: DynamicImage,
    format: PixelFormat,
    colour_map: Option<ColourMap>,
    _width: u16,
    _height: u16,
}

enum ColourFormat {
    U8((u8, u8, u8)),
    U16((u16, u16, u16)),
}

impl Image {
    fn new(format: PixelFormat, width: u16, height: u16) -> Result<Self> {
        let image = match (format.depth, format.true_colour) {
            (15, true) | (16, true) | (24, true) => {
                DynamicImage::ImageRgb8(ImageBuffer::<Rgb<u8>, Vec<u8>>::new(
                    width.into(),
                    height.into(),
                ))
            }
            (8, false) => DynamicImage::ImageRgb16(ImageBuffer::<
                Rgb<u16>,
                Vec<u16>,
            >::new(
                width.into(),
                height.into(),
            )),
            (d, t) => {
                return Err(eyre!(
                    "Invalid colour depth: {d}, true colour: {t}",
                ))
            }
        };

        Ok(Self {
            image,
            format,
            colour_map: None,
            _width: width,
            _height: height,
        })
    }

    fn put_pixels(
        &mut self,
        target: &Target,
        rect: Rect,
        pixels: &[u8],
    ) -> Result<()> {
        use ColourFormat::*;
        trace!(target, "pixels: {:?}", pixels);
        trace!(target, "rect: {:?}", rect);

        //debug!("rect: {:?}", rect);
        //debug!("number of pixels: {}", pixels.len());
        //5:37:08 [DEBUG] (4) scrying::vnc: rect: Rect {
        //  left: 1216,
        //  top: 704,
        //  width: 64,
        //  height: 16
        // }
        //15:37:08 [DEBUG] (4) scrying::vnc: number of pixels: 2048
        //
        // Each pixel is made out of two items from the pixels slice

        // Borrow the pixel format from self before mutably borrowing
        // the image
        let format = &self.format;
        let colour_map = &self.colour_map;

        // Rect { left: 1216, top: 704, width: 64, height: 16 }
        let bytes_per_pixel = match format.bits_per_pixel {
            8 => 1,
            16 => 2,
            32 => 4,
            _ => {
                return Err(eyre!(
                    "Invalid bits per pixel: {}",
                    format.bits_per_pixel
                ))
            }
        };
        let mut idx = 0_usize;
        for y in rect.top..(rect.top + rect.height) {
            for x in rect.left..(rect.left + rect.width) {
                trace!(
                    target,
                    "Position: {},{}: {:?}",
                    x,
                    y,
                    &pixels[idx..(idx + bytes_per_pixel)]
                );

                match &mut self.image {
                    DynamicImage::ImageRgb8(img) => {
                        if let U8((r, g, b)) = Image::pixel_to_rgb(
                            format,
                            colour_map,
                            &pixels[idx..(idx + bytes_per_pixel)],
                        )? {
                            img.put_pixel(x.into(), y.into(), Rgb([r, g, b]))
                        } else {
                            return Err(eyre!(
                                "Colour format mismatch: expected 8-bit colours",
                            ));
                        }
                    }
                    DynamicImage::ImageRgb16(img) => {
                        if let U16((r, g, b)) = Image::pixel_to_rgb(
                            format,
                            colour_map,
                            &pixels[idx..(idx + bytes_per_pixel)],
                        )? {
                            img.put_pixel(x.into(), y.into(), Rgb([r, g, b]))
                        } else {
                            return Err(eyre!(
                                "Colour format mismatch: expected 16-bit colours",
                            ));
                        }
                    }

                    _ => unimplemented!(),
                }

                idx += bytes_per_pixel;
            }
        }

        Ok(())
    }

    /// Convert two bytes of RGB16 into their corresponding r,g,b
    /// components according to the given pixel format
    ///
    /// −depth depth
    ///   Specify the pixel depth (in bits) of the VNC desktop to be
    ///   created. Default is 24. Other possible values are 8, 15 and 16
    ///   - anything else is likely to cause strange behaviour by
    ///   applications.
    ///
    /// −pixelformat format
    ///   Specify pixel format for Xvnc to use (BGRnnn or RGBnnn). The
    ///   default for depth 8 is BGR233 (meaning the most significant
    ///   two bits represent blue, the next three green, and the least
    ///   significant three represent red), the default for depth 16 is
    ///   RGB565, and the default for depth 24 is RGB888.
    ///
    ///  −cc 3
    ///   As an alternative to the default TrueColor visual, this allows
    ///   you to run an Xvnc server with a PseudoColor visual (i.e. one
    ///   which uses a color map or palette), which can be useful for
    ///   running some old X applications which only work on such a
    ///   display. Values other than 3 (PseudoColor) and 4 (TrueColor)
    ///   for the −cc option may result in strange behaviour, and
    ///   PseudoColor desktops must have an 8-bit depth.
    ///
    /// Ref: https://tigervnc.org/doc/vncserver.html
    ///
    /// $ Xvfb -screen 0 800x600x24 -ac &
    /// PixelFormat {
    ///   bits_per_pixel: 16,
    ///   depth: 16,
    ///   big_endian: false,
    ///   true_colour: true,
    ///   red_max: 31,
    ///   green_max: 63,
    ///   blue_max: 31,
    ///   red_shift: 11,
    ///   green_shift: 5,
    ///   blue_shift: 0
    /// }
    ///
    /// $ Xvfb -screen 0 800x600x16 -ac &
    /// PixelFormat {
    ///   bits_per_pixel: 32,
    ///   depth: 24,
    ///   big_endian: false,
    ///   true_colour: true,
    ///   red_max: 255,
    ///   green_max: 255,
    ///   blue_max: 255,
    ///   red_shift: 16,
    ///   green_shift: 8,
    ///   blue_shift: 0
    /// }
    ///
    /// Xvfb -screen 0 800x600x15 -ac &
    /// PixelFormat {
    ///   bits_per_pixel: 16,
    ///   depth: 15,
    ///   big_endian: false,
    ///   true_colour: true,
    ///   red_max: 31,
    ///   green_max: 31,
    ///   blue_max: 31,
    ///   red_shift: 10,
    ///   green_shift: 5,
    ///   blue_shift: 0
    /// }
    ///
    /// Xvfb -screen 0 800x600x8 -ac &
    /// PixelFormat {
    ///   bits_per_pixel: 8,
    ///   depth: 8,
    ///   big_endian: false,
    ///   true_colour: false,
    ///   red_max: 0,
    ///   green_max: 0,
    ///   blue_max: 0,
    ///   red_shift: 0,
    ///   green_shift: 0,
    ///   blue_shift: 0
    /// }
    /// This one results in Unsupported event: SetColourMap which we
    /// need to handle somehow

    //TODO unit test
    fn pixel_to_rgb(
        format: &PixelFormat,
        colour_map: &Option<ColourMap>,
        bytes: &[u8],
    ) -> Result<ColourFormat> {
        use ColourFormat::*;
        //TODO code reuse
        match (format.bits_per_pixel, format.depth) {
            (16, 16) | (16, 15) => {
                let bytes: [u8; 2] = bytes.try_into()?;
                let px = if format.big_endian {
                    u16::from_be_bytes(bytes)
                } else {
                    u16::from_le_bytes(bytes)
                };
                let blue_mask = format.blue_max as u16; // 5 bits
                let green_mask = format.green_max as u16; // 6 bits
                let red_mask = format.red_max as u16; // 5 bits

                let b = (px >> format.blue_shift) & blue_mask; // 0x1f
                let g = (px >> format.green_shift) & green_mask; // 0x3f
                let r = (px >> format.red_shift) & red_mask; // 0x1f

                // Left shift all the values so that they're at the top of their
                // respective bytes
                let b = b << (8 - blue_mask.count_ones()); // 3
                let g = g << (8 - green_mask.count_ones()); // 2
                let r = r << (8 - red_mask.count_ones()); // 3

                Ok(U8((r.try_into()?, g.try_into()?, b.try_into()?)))
            }
            (32, 24) => {
                let bytes: [u8; 4] = bytes.try_into()?;
                let px = if format.big_endian {
                    u32::from_be_bytes(bytes)
                } else {
                    u32::from_le_bytes(bytes)
                };
                let blue_mask = format.blue_max as u32; // 5 bits
                let green_mask = format.green_max as u32; // 6 bits
                let red_mask = format.red_max as u32; // 5 bits

                let b = (px >> format.blue_shift) & blue_mask; // 0x1f
                let g = (px >> format.green_shift) & green_mask; // 0x3f
                let r = (px >> format.red_shift) & red_mask; // 0x1f

                // Values do not need left shifting because they are
                // already 8-bits long

                Ok(U8((r.try_into()?, g.try_into()?, b.try_into()?)))
            }
            (8, 8) => {
                let px = bytes[0];
                if let Some(colour_map) = colour_map {
                    let colour = &colour_map.colours[px as usize];
                    let r = colour.red;
                    let g = colour.green;
                    let b = colour.blue;

                    Ok(U16((r, g, b)))
                } else {
                    Err(eyre!("No colour map supplied for 8-bit mode!",))
                }
            }
            d => panic!("Unsupported colour depth {:?}", d),
        }
    }

    fn set_colour_map(
        &mut self,
        first_colour: u16,
        colours: Vec<Colour>,
    ) -> Result<()> {
        if colours.len() != 256 {
            return Err(eyre!(
                "Invalid number of colours in map: {}",
                colours.len()
            ));
        }
        self.colour_map = Some(ColourMap {
            first_colour,
            colours,
        });

        Ok(())
    }
}

struct ColourMap {
    #[allow(unused)]
    first_colour: u16,
    colours: Vec<Colour>,
}

fn vnc_capture(
    target: &Target,
    opts: &Opts,
    report_tx: &Sender<ReportMessage>,
) -> Result<()> {
    info!(target, "Connecting to {:?}", target);
    let addr = match target {
        Target::Address(sock_addr) => sock_addr,
        Target::Url(_) => {
            return Err(eyre!("Invalid VNC target: {target}",));
        }
    };

    // timeout after 5s
    let duration = Duration::new(opts.conn_timeout, 0);
    let stream = TcpStream::connect_timeout(addr, duration)?; 

    let mut vnc = Client::from_tcp_stream(stream, false, |methods| {
        debug!(target, "available auth methods: {:?}", methods);
        // Turn off Clippy's single_match check because there might be
        // other auth methods in the future
        #[allow(clippy::single_match)]
        for method in methods {
            match method {
                AuthMethod::None => return Some(AuthChoice::None),
                AuthMethod::Password => {
                    let mut pass = [0_u8; 8];
                    if let Some(auth) = &opts.vnc_auth {
                        let auth_bytes = auth.as_bytes();
                        let passlen = min(8, auth_bytes.len());
                        pass[..passlen].copy_from_slice(&auth_bytes[..passlen]);
                    } else {
                        warn!(target, "Password requested but not provided");
                    }
                    return Some(AuthChoice::Password(pass));
                }
                _ => {}
            }
        }
        warn!(target, "AuthMethod::None may not be supported");
        None
    })?;

    // Server specifies the initial size, so just use that
    let (width, height) = vnc.size();
    info!(
        target,
        "connected to \"{}\", {}x{} framebuffer",
        vnc.name(),
        width,
        height
    );

    vnc.set_encodings(&[
        vnc::Encoding::Zrle,
        vnc::Encoding::CopyRect,
        vnc::Encoding::Raw,
        vnc::Encoding::Cursor,
        vnc::Encoding::DesktopSize,
    ])?;

    let vnc_format = vnc.format();

    debug!(target, "VNC pixel format: {:?}", vnc_format);

    debug!(target, "requesting update");
    vnc.request_update(
        vnc::Rect {
            left: 0,
            top: 0,
            width,
            height,
        },
        false,
    )?;

    if opts.move_mouse {
        info!(target, "Sending mouse event");
        vnc.send_pointer_event(0, 0, 0)?;
    }

    let mut vnc_image = Image::new(vnc_format, width, height)?;

    vnc_poll(target, vnc, &mut vnc_image)?;

    // Save the image
    info!(target, "Successfully received image");
    let filename = format!("{}.png", target_to_filename(target));
    let relative_filepath = Path::new("vnc").join(&filename);
    let filepath = Path::new(&opts.output_dir).join(&relative_filepath);
    info!(target, "Saving image as {}", filepath.display());
    vnc_image.image.save(&filepath)?;

    let report_message = ReportMessage::Output(ReportMessageContent {
        mode: Vnc,
        target: target.to_string(),
        output: FileError::File(relative_filepath.display().to_string()),
    });
    report_tx.send(report_message)?;

    Ok(())
}

fn vnc_poll(
    target: &Target,
    mut vnc: Client,
    vnc_image: &mut Image,
) -> Result<()> {
    use vnc::client::Event::*;
    loop {
        for event in vnc.poll_iter() {
            match event {
                Disconnected(None) => {
                    warn!(target, "VNC Channel disconnected");
                    return Ok(());
                }
                PutPixels(vnc_rect, ref pixels) => {
                    trace!(target, "PutPixels");
                    vnc_image.put_pixels(target, vnc_rect, pixels)?;
                }
                EndOfFrame => {
                    debug!(target, "End of frame");
                    return Ok(());
                }
                SetColourMap {
                    first_colour,
                    colours,
                } => {
                    debug!(target, "Set colour map");
                    trace!(target, "first colour: {:x}", first_colour);
                    trace!(target, "colours: {:?}", colours);
                    vnc_image.set_colour_map(first_colour, colours)?;
                }
                other => debug!(target, "Unsupported event: {:?}", other),
            }
        }
    }
}

pub fn capture(
    target: &Target,
    opts: &Opts,
    tx: Sender<ThreadStatus>,
    report_tx: &Sender<ReportMessage>,
) {
    if let Err(e) = vnc_capture(target, opts, report_tx) {
        warn!(target, "VNC error: {}", e);
    }

    tx.send(ThreadStatus::Complete).unwrap();
}
