// 60Hz X11 animation load generator: scrolling text-like pattern + moving box.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <sys/shm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdint.h>
#include <sys/time.h>
static void marker(uint32_t*dst,int stride,long f){ struct timeval tv; gettimeofday(&tv,0); uint32_t ms=(uint32_t)((tv.tv_sec*1000LL+tv.tv_usec/1000)&0xffffff); uint64_t v=((uint64_t)(f&0xffff)<<24)|ms; for(int b=0;b<40;b++){ int bit=(v>>b)&1; int bx=(b%20)*24, by=(b/20)*24; for(int y=by;y<by+24;y++) for(int x=bx;x<bx+24;x++) dst[(size_t)y*stride+x]=bit?0xffffffff:0xff000000; } }
static double now(){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc,char**argv){
  int W=argc>1?atoi(argv[1]):1920,H=argc>2?atoi(argv[2]):1080; double hz=argc>3?atof(argv[3]):60; int secs=argc>4?atoi(argv[4]):30;
  Display*d=XOpenDisplay(NULL); if(!d){fprintf(stderr,"no display\n");return 1;}
  int s=DefaultScreen(d);
  Window w=XCreateSimpleWindow(d,RootWindow(d,s),argc>5?atoi(argv[5]):300,argc>6?atoi(argv[6]):150,W,H,0,0,0);
  XSetWindowAttributes a; a.override_redirect=True; XChangeWindowAttributes(d,w,CWOverrideRedirect,&a);
  XMapRaised(d,w); XSync(d,0);
  XShmSegmentInfo si; XImage*img=XShmCreateImage(d,DefaultVisual(d,s),24,ZPixmap,NULL,&si,W,H);
  si.shmid=shmget(IPC_PRIVATE,img->bytes_per_line*H,IPC_CREAT|0600); si.shmaddr=img->data=shmat(si.shmid,0,0); si.readOnly=0;
  XShmAttach(d,&si); shmctl(si.shmid,IPC_RMID,0);
  GC gc=XCreateGC(d,w,0,0);
  // static "document" texture taller than window
  int TH=H*4; uint32_t*tex=malloc((size_t)W*TH*4);
  srand(1);
  for(int y=0;y<TH;y++){ int line=y/24, iny=y%24; for(int x=0;x<W;x++){ uint32_t c=0xfff5f5f5;
      if(iny>4&&iny<18 && x>40 && x< 40+ (int)((line*7919)%(W-200)) ){ int glyph=(x/9 + line*31)%7; int gx=x%9; if(glyph && gx<7 && ((x*13+y*7+line)%5)<3) c=0xff202020+ (line%3)*0x200000; }
      if(line%17==0) c=0xff3070d0; tex[(size_t)y*W+x]=c; } }
  double t0=now(), next=t0; long f=0;
  while(now()-t0<secs){
    int off=(int)(f*6)%(TH-H);
    uint32_t*dst=(uint32_t*)img->data;
    for(int y=0;y<H;y++) memcpy(dst+(size_t)y*(img->bytes_per_line/4), tex+(size_t)(y+off)*W, W*4);
    int bx=(int)(f*8)%(W-200), by=H/2-100; for(int y=by;y<by+200;y++) for(int x=bx;x<bx+200;x++) dst[(size_t)y*(img->bytes_per_line/4)+x]=0xffe04040 ^ (f&0xff);
    marker(dst,img->bytes_per_line/4,f); XShmPutImage(d,w,gc,img,0,0,0,0,W,H,False); XSync(d,0); f++;
    next+=1.0/hz; double sl=next-now(); if(sl>0){struct timespec ts={0,(long)(sl*1e9)}; nanosleep(&ts,0);} 
  }
  fprintf(stderr,"loadgen frames=%ld fps=%.1f\n",f,f/(now()-t0));
  return 0;
}
